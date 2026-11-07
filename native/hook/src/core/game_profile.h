/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The games the hook knows, by PE identity (OD-206). FM5 starts only from ExtraOptions.Infinity's
   game-map callback (azimuth_tls.c), which selects the profile once, before anything else runs.
   Nothing selected means FH2, so FH2's behaviour and the hook's FH2-only tests are unchanged.
   Header only: every module, and every test that includes a module's source, gets it as it is. */
#ifndef FI_GAME_PROFILE_H
#define FI_GAME_PROFILE_H

typedef enum { FI_GAME_FH2 = 1, FI_GAME_FM5 = 2 } FiGameId;

typedef struct {
    FiGameId id;
    char name[4];
    unsigned timestamp, image_size;   /* PE TimeDateStamp and SizeOfImage, as Azimuth maps it */
    const wchar_t *title_section;     /* its section in data\azimuth.ini */
    const wchar_t *car_db;            /* the car database, relative to Mount */
    int private_hopper;               /* constants.custom.hopperId of its private session write */
} FiGameProfile;

static const FiGameProfile fi_game_profiles[] = {
    {FI_GAME_FH2, "FH2", GAME_TIMESTAMP, GAME_IMAGE_SIZE, L"title-1a96545b", L"media\\stripped\\gamedbRC.slt", 2},
    {FI_GAME_FM5, "FM5", 0x53d8d1f2u, 0x279d000u, L"title-7b35d917", L"media\\db\\gamedb.slt", -1},
};

__declspec(selectany) const FiGameProfile *volatile fi_game_selected;
__declspec(selectany) unsigned char *volatile fi_game_selected_image;

static inline const FiGameProfile *fi_game_profile_lookup(unsigned timestamp, unsigned image_size) {
    for (unsigned i = 0; i < sizeof(fi_game_profiles) / sizeof(fi_game_profiles[0]); ++i) {
        if (fi_game_profiles[i].timestamp == timestamp && fi_game_profiles[i].image_size == image_size) {
            return &fi_game_profiles[i];
        }
    }
    return 0;
}

static inline const FiGameProfile *fi_game_profile(void) {
    const FiGameProfile *selected = fi_game_selected;
    return selected ? selected : &fi_game_profiles[0];
}

static inline int fi_game_is_fh2(void) {
    return fi_game_profile()->id == FI_GAME_FH2;
}

/* A JSON integer at `p`: optional '-', at most 6 digits, after optional white space. */
static inline int fi_hopper_parse(const unsigned char *p, const unsigned char *end, int *value) {
    while (p < end && (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n')) ++p;
    int negative = p < end && *p == '-', number = 0;
    unsigned digits = 0;
    if (negative) ++p;
    for (; p < end && *p >= '0' && *p <= '9' && digits < 6; ++p, ++digits) number = number * 10 + (*p - '0');
    if (!digits || (p < end && *p >= '0' && *p <= '9')) return 0;
    *value = negative ? -number : number;
    return 1;
}
#endif
