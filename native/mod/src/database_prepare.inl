namespace {
struct SqlRegistration {
    InfinityMod owner;
    std::vector<std::string> sql, tables;
    bool accepting = false;
};
std::vector<std::unique_ptr<SqlRegistration>> sql_registrations;
DWORD sql_registration_thread = 0;
int queue_startup_sql(InfinityMod* mod, const char* sql) {
    if (GetCurrentThreadId() != sql_registration_thread) return 0;
    for (auto& r : sql_registrations) if (&r->owner == mod && r->accepting) {
        if (!sql || r->sql.size() >= 64 || r->tables.empty()) return 0;
        auto length = strnlen_s(sql, 65537); if (!length || length > 65536) return 0;
        r->sql.emplace_back(sql, length); return 1;
    }
    return 0;
}
int sql_authorize(void* context, int action, const char* table, const char*, const char* database, const char* trigger) {
    const auto& r = *static_cast<SqlRegistration*>(context);
    if (trigger || (database && std::strcmp(database, "main"))) return SQLITE_DENY;
    if (action == SQLITE_SELECT) return SQLITE_OK;
    if (action != SQLITE_READ && action != SQLITE_UPDATE && action != SQLITE_INSERT && action != SQLITE_DELETE) return SQLITE_DENY;
    if (!table) return SQLITE_DENY;
    for (const auto& allowed : r.tables) if (sqlite3_stricmp(table, allowed.c_str()) == 0) return SQLITE_OK;
    return SQLITE_DENY;
}
void sql_exec(sqlite3* db, const char* sql) {
    if (sqlite3_exec(db, sql, nullptr, nullptr, nullptr) != SQLITE_OK) throw std::runtime_error("SQL transaction refused");
}
struct SqlDatabase {
    sqlite3* handle = nullptr;
    ~SqlDatabase() { if (handle) sqlite3_close_v2(handle); }
    void open(const std::filesystem::path& path, int flags) {
        if (sqlite3_open_v2(infinity_mod::narrow(path.wstring()).c_str(), &handle, flags | SQLITE_OPEN_FULLMUTEX, nullptr) != SQLITE_OK)
            throw std::runtime_error("database open failed");
        sqlite3_db_config(handle, SQLITE_DBCONFIG_DEFENSIVE, 1, nullptr);
        sqlite3_db_config(handle, SQLITE_DBCONFIG_TRUSTED_SCHEMA, 0, nullptr);
        sqlite3_limit(handle, SQLITE_LIMIT_LENGTH, 1024 * 1024);
        sqlite3_limit(handle, SQLITE_LIMIT_SQL_LENGTH, 65536);
        sqlite3_limit(handle, SQLITE_LIMIT_ATTACHED, 0);
        sqlite3_busy_timeout(handle, 2000);
    }
};
void hash_bytes(const uint8_t* bytes, size_t count, uint8_t result[32]) {
    BCRYPT_ALG_HANDLE algorithm{};
    if (count > ULONG_MAX || BCryptOpenAlgorithmProvider(&algorithm, BCRYPT_SHA256_ALGORITHM, nullptr, 0) < 0) throw std::runtime_error("hash failed");
    auto status = BCryptHash(algorithm, nullptr, 0, const_cast<PUCHAR>(bytes), static_cast<ULONG>(count), result, 32);
    BCryptCloseAlgorithmProvider(algorithm, 0);
    if (status < 0) throw std::runtime_error("hash failed");
}
std::vector<uint8_t> read_bounded(HANDLE file, size_t max) {
    LARGE_INTEGER size{}, zero{};
    if (!GetFileSizeEx(file, &size) || size.QuadPart < 1 || static_cast<uint64_t>(size.QuadPart) > max
        || !SetFilePointerEx(file, zero, nullptr, FILE_BEGIN)) throw std::runtime_error("invalid file size");
    std::vector<uint8_t> bytes(size_t(size.QuadPart)); DWORD count = 0;
    if (!ReadFile(file, bytes.data(), static_cast<DWORD>(bytes.size()), &count, nullptr) || count != bytes.size()) throw std::runtime_error("file read failed");
    return bytes;
}
void durable_file(const std::filesystem::path& path, const uint8_t* bytes, DWORD count) {
    auto file = CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (file == INVALID_HANDLE_VALUE) throw std::runtime_error("output already exists");
    DWORD wrote = 0; bool ok = WriteFile(file, bytes, count, &wrote, nullptr) && wrote == count && FlushFileBuffers(file);
    CloseHandle(file); if (!ok) throw std::runtime_error("output write failed");
}
int guarded_register(InfinityModRegisterDatabaseV1Fn entry, const InfinityModDatabaseApiV1* api) {
    __try { return entry(api); } __except (EXCEPTION_EXECUTE_HANDLER) { return 1; }
}
void apply_registered_sql(const std::filesystem::path& input, const std::filesystem::path& output, uint8_t sql_hash[32]) {
    if (std::filesystem::exists(output)) throw std::runtime_error("existing session database");
    SqlDatabase source, target;
    source.open(input, SQLITE_OPEN_READONLY); target.open(output, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE);
    auto backup = sqlite3_backup_init(target.handle, "main", source.handle, "main");
    if (!backup) throw std::runtime_error("backup failed");
    auto copied = sqlite3_backup_step(backup, -1); auto ended = sqlite3_backup_finish(backup);
    if (copied != SQLITE_DONE || ended != SQLITE_OK) throw std::runtime_error("backup failed");
    sql_exec(target.handle, "PRAGMA journal_mode=DELETE; PRAGMA foreign_keys=ON;");
    std::vector<uint8_t> transcript;
    auto append = [&](const std::string& s) { uint32_t size = static_cast<uint32_t>(s.size()); auto p = reinterpret_cast<uint8_t*>(&size); transcript.insert(transcript.end(), p, p + 4); transcript.insert(transcript.end(), s.begin(), s.end()); };
    for (auto& r : sql_registrations) {
        append("mod"); append(r->owner.name); append(std::to_string(r->tables.size()));
        for (auto& table : r->tables) append(table);
        append(std::to_string(r->sql.size()));
        sql_exec(target.handle, "BEGIN IMMEDIATE");
        sqlite3_set_authorizer(target.handle, sql_authorize, r.get());
        ULONGLONG deadline = GetTickCount64() + 5000;
        sqlite3_progress_handler(target.handle, 1000, [](void* value) { return GetTickCount64() >= *static_cast<ULONGLONG*>(value) ? 1 : 0; }, &deadline);
        try {
            for (auto& sql : r->sql) { append(sql); sql_exec(target.handle, sql.c_str()); }
            sqlite3_progress_handler(target.handle, 0, nullptr, nullptr);
            sqlite3_set_authorizer(target.handle, nullptr, nullptr);
            sql_exec(target.handle, "COMMIT");
        } catch (...) {
            sqlite3_progress_handler(target.handle, 0, nullptr, nullptr); sqlite3_set_authorizer(target.handle, nullptr, nullptr);
            sqlite3_exec(target.handle, "ROLLBACK", nullptr, nullptr, nullptr); throw;
        }
    }
    sqlite3_stmt* check = nullptr;
    if (sqlite3_prepare_v2(target.handle, "PRAGMA integrity_check", -1, &check, nullptr) != SQLITE_OK) throw std::runtime_error("integrity failed");
    bool ok = sqlite3_step(check) == SQLITE_ROW && std::strcmp(reinterpret_cast<const char*>(sqlite3_column_text(check, 0)), "ok") == 0;
    sqlite3_finalize(check); if (!ok) throw std::runtime_error("integrity failed");
    if (sqlite3_prepare_v2(target.handle, "PRAGMA foreign_key_check", -1, &check, nullptr) != SQLITE_OK) throw std::runtime_error("foreign keys failed");
    ok = sqlite3_step(check) == SQLITE_DONE; sqlite3_finalize(check);
    if (!ok) throw std::runtime_error("foreign keys failed");
    hash_bytes(transcript.data(), transcript.size(), sql_hash);
}
void prepare_session_database(ParsedPlan& plan, const std::filesystem::path& folder) {
    if (!valid_path(plan.azimuth.c_str()) || !valid_path(plan.game.c_str()) || nonzero(plan.prepared_hash) || nonzero(plan.sql_hash)) throw std::runtime_error("invalid preparation plan");
    sql_registration_thread = GetCurrentThreadId();
    auto input = folder / L"baseline.sqlite", output = folder / L"session.sqlite";
    auto source = verify_file(input.wstring(), plan.baseline_hash); if (!source) throw std::runtime_error("baseline mismatch");
    auto header = read_bounded(source->file, 64 * 1024 * 1024);
    if (header.size() < 100 || std::memcmp(header.data(), "SQLite format 3\0", 16) || header[18] != 1 || header[19] != 1) throw std::runtime_error("unsupported journal mode");
    for (auto suffix : { L"-wal", L"-shm", L"-journal" }) if (std::filesystem::exists(input.wstring() + suffix)) throw std::runtime_error("database sidecar present");
    std::vector<std::unique_ptr<FileLock>> locks;
    for (uint32_t i = 0; i < plan.count; ++i) {
        auto& item = plan.mods[i];
        if (!identity(item.id, false) || !identity(item.version, true)) throw std::runtime_error("invalid mod identity");
        auto prefix = plan.azimuth + L"\\";
        if (_wcsnicmp(item.binary_path, prefix.c_str(), prefix.size()) || _wcsnicmp(item.settings_path, prefix.c_str(), prefix.size())) throw std::runtime_error("mod outside installation");
        auto binary = verify_file(item.binary_path, item.binary_sha256), settings = verify_file(item.settings_path, item.settings_sha256);
        if (!binary || !settings) throw std::runtime_error("mod input mismatch");
        locks.push_back(std::move(binary)); locks.push_back(std::move(settings));
    }
    for (uint32_t i = 0; i < plan.count; ++i) {
        auto& item = plan.mods[i];
        auto r = std::make_unique<SqlRegistration>(); r->owner.name = item.id; r->tables = plan.tables[i];
        auto* registration = r.get(); sql_registrations.push_back(std::move(r));
        if (registration->tables.empty()) continue;
        auto module = LoadLibraryExW(item.binary_path, nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!module) throw std::runtime_error("mod load failed");
        auto entry = reinterpret_cast<InfinityModRegisterDatabaseV1Fn>(GetProcAddress(module, "InfinityModRegisterDatabaseV1"));
        if (!entry) throw std::runtime_error("database registration export missing");
        InfinityModDatabaseApiV1 api{sizeof api, 1, &registration->owner, item.settings_path, queue_startup_sql};
        registration->accepting = true; auto result = guarded_register(entry, &api); registration->accepting = false;
        if (result) throw std::runtime_error("mod refused database registration");
    }
    uint8_t hashes[64]{};
    apply_registered_sql(input, output, hashes + 32);
    auto file = CreateFileW(output.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT, nullptr);
    if (!plain(file, false)) { if (file != INVALID_HANDLE_VALUE) CloseHandle(file); throw std::runtime_error("invalid output"); }
    std::vector<uint8_t> bytes;
    try { bytes = read_bounded(file, 64 * 1024 * 1024); } catch (...) { CloseHandle(file); throw; }
    CloseHandle(file); hash_bytes(bytes.data(), bytes.size(), hashes);
    durable_file(folder / L"database.hashes", hashes, sizeof hashes);
}
}
extern "C" __declspec(dllexport) int InfinityModsPrepareDatabaseV1(const wchar_t* path, const uint8_t hash[32]) {
    try {
        wchar_t exe[32768]; DWORD n = GetModuleFileNameW(nullptr, exe, 32768);
        if (!n || n >= 32768 || _wcsicmp(std::filesystem::path(exe).filename().c_str(), L"infinity-mod-prepare.exe")) return 1;
        auto file = verify_file(path, hash); if (!file || !sql_registrations.empty()) return 1;
        auto bytes = read_bounded(file->file, 1024 * 1024); ParsedPlan plan; parse_plan(bytes, plan);
        prepare_session_database(plan, std::filesystem::path(path).parent_path()); return 0;
    } catch (...) { return 1; }
}
