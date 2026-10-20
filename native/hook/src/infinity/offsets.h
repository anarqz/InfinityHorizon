/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE). */
#ifndef FH2_AZIMUTH_OFFSETS_H
#define FH2_AZIMUTH_OFFSETS_H

/* Where FH2Online's adapters hook azimuth.exe, per Azimuth build. 0.9.9's were found from
   0.9.7's, and 0.9.10's from 0.9.9's, by matching functions instruction by instruction (address
   operands blanked); each function compared equal, and the hashed bytes are checked again before
   anything is written. */
typedef struct {
    const char *name;
    /* azimuth_net.c: CoCreateInstance (CLSID_FreeThreadedXMLHTTP60 -> OfflineRequest) */
    unsigned net_hook;
    const unsigned char *net_hook_hash; /* SHA-256 of its first 0x20 bytes */
    unsigned net_mmdevice_factory;
    /* the WinRT activation trace (GetActivationFactoryByPCWSTR) and its providers in order */
    unsigned net_activation;
    const unsigned char *net_activation_hash; /* SHA-256 of its first 0x20 bytes */
    unsigned net_activation_providers[7];
    /* User.GetTokenAndSignature* slots of the two User vtables and the functions they hold */
    unsigned net_token_slots[6], net_token_targets[6];
    /* WindowsCreateString */
    unsigned hstring_create;
    /* azimuth_account.c: the player-name std::wstring, XboxUserId, LocalDisplayInfo */
    unsigned account_name;
    unsigned account_xuid_slot, account_xuid_target;
    unsigned account_info_slots[3], account_info_targets[3];
} AzimuthOffsets;

/* The offsets for this azimuth.exe, or 0 for a build FH2Online was not made for. */
const AzimuthOffsets *azimuth_offsets(const unsigned char *base);

#endif
