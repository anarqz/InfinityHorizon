/* ForzaInfinity hook. Derived from Forza Horizon 2 Extra Options by Arthur (RokkuDayo),
   https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions (upstream commit c2e3298, 0.2.74),
   GPL-3.0-or-later. See LICENSE and NOTICE.md.

   data\bin\infinity.log: rewritten at every start (the banner, the status line, then one line
   per event, each starting with the local time "[HH:MM:SS.mmm] " so two PCs' logs line up), the
   last start's kept as infinity.previous.log. */
#include "logging.h"
#include "support.h"

static wchar_t runtime_log_path[PATH_CAPACITY];
static wchar_t startup_log_path[PATH_CAPACITY];
static int log_enabled = 1;
static volatile LONG log_at_line_start = 1; /* a line written in pieces gets one time stamp */

static void write_text(HANDLE file, const char *text);

/* "[HH:MM:SS.mmm] " when the file is at the start of a line. */
static void write_stamp(HANDLE file) {
    if (!InterlockedExchange(&log_at_line_start, 0)) {
        return;
    }
    SYSTEMTIME now;
    GetLocalTime(&now);
    char stamp[16];
    const unsigned parts[4] = {now.wHour, now.wMinute, now.wSecond, now.wMilliseconds};
    size_t n = 0;
    stamp[n++] = '[';
    for (int i = 0; i < 4; ++i) {
        const unsigned v = parts[i];
        if (i == 3) {
            stamp[n++] = (char)('0' + v / 100 % 10);
        }
        stamp[n++] = (char)('0' + v / 10 % 10);
        stamp[n++] = (char)('0' + v % 10);
        stamp[n++] = i < 2 ? ':' : i == 2 ? '.' : ']';
    }
    stamp[n++] = ' ';
    stamp[n] = 0;
    write_text(file, stamp);
}

static void note_line_end(const char *text) {
    const size_t n = text_length(text);
    InterlockedExchange(&log_at_line_start, n && text[n - 1] == '\n' ? 1 : 0);
}

void log_reset(void) {
    runtime_log_path[0] = 0;
}

void log_set_enabled(int enabled) {
    log_enabled = enabled != 0;
    if (!log_enabled) {
        runtime_log_path[0] = 0;
    }
}

void log_set_directory(const wchar_t *directory) {
    if (!log_enabled || !path_join(runtime_log_path, directory, MOD_LOG_NAME)) {
        runtime_log_path[0] = 0;
    }
}

void log_append(const char *message) {
    if (!runtime_log_path[0]) {
        return;
    }
    HANDLE file =
        CreateFileW(runtime_log_path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                    OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE) {
        return;
    }
    LARGE_INTEGER length;
    if(!GetFileSizeEx(file,&length) || length.QuadPart>=8*1024*1024) {CloseHandle(file);return;}
    write_stamp(file);
    DWORD bytes_written;
    WriteFile(file, message, (DWORD)text_length(message), &bytes_written, NULL);
    note_line_end(message);
    CloseHandle(file);
}

static void write_text(HANDLE file, const char *text) {
    DWORD bytes_written;
    WriteFile(file, text, (DWORD)text_length(text), &bytes_written, NULL);
}

static void write_number(HANDLE file, int value) {
    char number[12];
    char *cursor = number + sizeof(number) - 1;
    unsigned magnitude = value < 0 ? 0u - (unsigned)value : (unsigned)value;
    *cursor = 0;
    do {
        *--cursor = (char)('0' + magnitude % 10);
        magnitude /= 10;
    } while (magnitude);
    if (value < 0) {
        *--cursor = '-';
    }
    write_text(file, cursor);
}

void log_append_value(const char *label, int value) {
    if (!runtime_log_path[0]) {
        return;
    }
    HANDLE file =
        CreateFileW(runtime_log_path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL,
                    OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file != INVALID_HANDLE_VALUE) {
        write_stamp(file);
        write_text(file, label);
        write_number(file, value);
        write_text(file, "\r\n");
        InterlockedExchange(&log_at_line_start, 1);
        CloseHandle(file);
    }
}

void log_start(const wchar_t *directory) {
    if (!path_join(startup_log_path, directory, MOD_LOG_NAME)) {
        return;
    }
    /* The last launch's log is kept as infinity.previous.log (once a launch): a game that froze
       or crashed is started again before anyone looks, and a report wants both. */
    static int rotated;
    static wchar_t previous_path[PATH_CAPACITY];
    if (!rotated && path_join(previous_path, directory, MOD_PREVIOUS_LOG_NAME)) {
        rotated = 1;
        static wchar_t old_path[PATH_CAPACITY],new_path[PATH_CAPACITY];
        for(unsigned i=5;i>=2;--i) {
            wchar_t from[]=L"infinity.previous-1.log",to[]=L"infinity.previous-1.log";
            from[18]=(wchar_t)('0'+i-1);to[18]=(wchar_t)('0'+i);
            if(path_join(old_path,directory,i==2?MOD_PREVIOUS_LOG_NAME:from) && path_join(new_path,directory,to))
                MoveFileExW(old_path,new_path,MOVEFILE_REPLACE_EXISTING);
        }
        MoveFileExW(startup_log_path, previous_path, MOVEFILE_REPLACE_EXISTING);
    }
    if (!log_enabled) {
        return;
    }
    HANDLE file = CreateFileW(startup_log_path, GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS,
                              FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE) {
        return;
    }
    write_text(file, MOD_BANNER "\r\n");
    write_text(file, status_get());
    write_text(file, "\r\n");
    CloseHandle(file);
}
