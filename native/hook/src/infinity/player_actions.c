/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).

   The game's own PLAYER ACTIONS popup gets ADD / REMOVE / ACCEPT / CANCEL FRIEND and REPORT PLAYER
   (OD-193 F-2; RE: player-actions.md, F-1). FH2 has none of these (the console's system card had
   them). The hook holds no friend or report logic: it asks the overlay the launcher loaded
   (fi_overlay_player_state / _action / _result, optional exports that never wait), and the launcher
   holds the account's friend list and makes the calls. Everything below runs on the game's UI
   thread, inside the popup's own functions:

   - text lookup (game+0x123d780, detour): keys whose high half is 0x4649 ("FI") are ours; the full
     32-bit key is checked (the game's keys are 16-bit hashes). Our labels and messages in English,
     or pt-BR when the game runs in it (lang.c);
   - the list commit (game+0xe1b5a0, detour), shared by many popups: for PopupPlayerAction (its
     vtable) our items are appended to the collection the builder made, with the game's own item
     (0xdf3730) and append (0xdfec20) calls, then the original commits. The friend item follows the
     launcher's state for the target's XUID (unknown: none); REPORT PLAYER for anyone but this
     profile. The REPORT reasons are an in-place sub-list: the item mask (+0x4e0) lets only the
     three reason ids through, then the game's own rebuild (0xe13be0) runs;
   - the selection (game+0xe080a0, detour): the original stores an id it does not know in +0x4c8
     and returns; ours are read back there and handled here. The dispatch (0xe1e920) never sees our
     ids (its default hands them to the menu's owner);
   - the popup's per-frame method (vtable slot 153): the original, then the answer of a pending
     action, shown in the game's own message popup (0xd09db0) with its result tag 3 (the game's own
     use: the parent closes on OK), or a rebuild when the launcher's state changed;
   - FriendsModule's update (its vtable slot 4): after a friend change it sets the module's queue
     flag (+0x23a, QueueFriendRetrievalRequest), so the game reads its friends again at once.

   Installed once, after the game build (TimeDateStamp, SizeOfImage), every byte it replaces or
   calls and every slot were checked; any mismatch leaves the whole game untouched (no item). */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"

#define PA_TEXT_RVA 0x123d780u
#define PA_COMMIT_RVA 0xe1b5a0u
#define PA_SELECT_RVA 0xe080a0u
#define PA_FRAME_RVA 0xe10b70u
#define PA_VTABLE_RVA 0x2e2d698u                  /* PopupPlayerAction::vftable */
#define PA_FRAME_SLOT_RVA (PA_VTABLE_RVA + 153u * 8u) /* 0x2e2db60 */
#define PA_ANSWER_SLOT_RVA (PA_VTABLE_RVA + 161u * 8u) /* 0x2e2dba0: a child popup's answer */
#define PA_ANSWER_RVA 0xe0dbb0u
#define PA_ITEM_RVA 0xdf3730u
#define PA_APPEND_RVA 0xdfec20u
#define PA_LOC_RVA 0x123a570u
#define PA_MESSAGE_RVA 0xd09db0u
#define PA_REBUILD_RVA 0xe13be0u
#define PA_FRIENDS_VTABLE_RVA 0x2c985e8u          /* EntityModel::FriendsModule::vftable */
#define PA_FRIENDS_SLOT_RVA (PA_FRIENDS_VTABLE_RVA + 4u * 8u)
#define PA_FRIENDS_UPDATE_RVA 0x551c10u

#define PA_SELECTED 0x4c8u     /* int: the chosen item's id */
#define PA_COMMITTED 0x490u    /* the list the popup shows now */
#define PA_ENTRY 0x4d0u        /* the target entry; its +0x78 the player object */
#define PA_MASK 0x4e0u         /* u32: an item is added only if mask & id */
#define PA_ENTRY_PLAYER 0x78u
#define PA_PLAYER_XUID_SLOT (0x38u / 8u)
#define PA_LIST_IN_COLLECTION 0x38u
#define PA_REFCOUNT 0x10u
#define PA_RELEASE_SLOT (0x18u / 8u)
#define PA_POPUP_RESULT 0x2d0u /* u64 a child popup's result for slot 161; 3: close the parent */
#define PA_FRIENDS_QUEUE 0x23au
#define PA_KEY_HIGH 0x46490000u
#define PA_TITLE_KEY 0x0c4fu   /* Lobby: PLAYER ACTIONS */

/* our item ids: above every native one (0x1 .. 0x1000) and let through by the ctor's mask */
#define PA_ID_ADD 0x00010000u
#define PA_ID_REMOVE 0x00020000u
#define PA_ID_ACCEPT 0x00040000u
#define PA_ID_CANCEL 0x00080000u
#define PA_ID_REPORT 0x00100000u
#define PA_ID_CHEATING 0x00200000u
#define PA_ID_ABUSE 0x00400000u
#define PA_ID_OTHER 0x00800000u
#define PA_REASON_BITS (PA_ID_CHEATING | PA_ID_ABUSE | PA_ID_OTHER)
#define PA_OUR_BITS 0x00ff0000u

/* fi_overlay.h */
#define PA_STATE_UNKNOWN 0u
#define PA_STATE_NONE 1u
#define PA_STATE_FRIEND 2u
#define PA_STATE_INCOMING 3u
#define PA_STATE_OUTGOING 4u
#define PA_STATE_OWN 5u
#define PA_ACT_ADD 1u
#define PA_ACT_REMOVE 2u
#define PA_ACT_ACCEPT 3u
#define PA_ACT_CANCEL 4u
#define PA_ACT_REPORT 5u
#define PA_RESULT_NONE 0u
#define PA_RESULT_REPORT_SUBMITTED 1u
#define PA_RESULT_REPORT_FAILED 2u
#define PA_RESULT_TRY_LATER 3u
#define PA_RESULT_REQUEST_SENT 4u
#define PA_RESULT_FRIENDS 5u
#define PA_RESULT_REMOVED 6u
#define PA_RESULT_CANCELLED 7u
#define PA_RESULT_NOT_FOUND 8u
#define PA_RESULT_LIMIT 9u
#define PA_RESULT_FAILED 10u

#define PA_POPUPS 4u
#define PA_STALE_MS 2000u    /* a popup not drawn this long is a new one when it builds again */
#define PA_PENDING_MS 65000u /* the overlay forgets an unanswered action after 60 s */

/* text lookup: mov r9d,r8d; cmp dx,ax; the 15 bytes moved (no RIP-relative operand), then jz +0x6a */
static const unsigned char pa_text_bytes[17] = {0x48, 0x83, 0xec, 0x38, 0xb8, 0xff, 0xff, 0x00, 0x00,
                                                0x45, 0x8b, 0xc8, 0x66, 0x3b, 0xd0, 0x74, 0x6a};
/* list commit: mov [rsp+8],rbx; mov [rsp+10h],rsi; push rdi; sub rsp,20h | xor esi,esi */
static const unsigned char pa_commit_bytes[17] = {0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24,
                                                  0x10, 0x57, 0x48, 0x83, 0xec, 0x20, 0x33, 0xf6};
/* selection: mov [rsp+10h],rbx; mov [rsp+18h],rsi; push rdi; sub rsp,20h | mov rbx,rcx */
static const unsigned char pa_select_bytes[18] = {0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24,
                                                  0x18, 0x57, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9};
#define PA_MOVED 15u
/* per-frame: push rbx; sub rsp,20h; mov rbx,rcx; call */
static const unsigned char pa_frame_bytes[10] = {0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9, 0xe8};
/* the functions called, by their first bytes */
static const unsigned char pa_item_bytes[19] = {0x40, 0x57, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xec, 0x40,
                                                0x48, 0xc7, 0x44, 0x24, 0x28, 0xfe, 0xff, 0xff, 0xff};
static const unsigned char pa_append_bytes[20] = {0x4c, 0x89, 0x44, 0x24, 0x18, 0x48, 0x89, 0x54, 0x24, 0x10,
                                                  0x57, 0x48, 0x83, 0xec, 0x30, 0x48, 0xc7, 0x44, 0x24, 0x20};
static const unsigned char pa_loc_bytes[20] = {0x40, 0x57, 0x48, 0x83, 0xec, 0x30, 0x48, 0xc7, 0x44, 0x24,
                                               0x28, 0xfe, 0xff, 0xff, 0xff, 0x48, 0x89, 0x5c, 0x24, 0x48};
static const unsigned char pa_message_bytes[23] = {0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89,
                                                   0x74, 0x24, 0x18, 0x57, 0x48, 0x83, 0xec, 0x20, 0x41, 0x8b, 0xe9};
static const unsigned char pa_rebuild_bytes[24] = {0x88, 0x54, 0x24, 0x10, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55,
                                                   0x41, 0x56, 0x41, 0x57, 0x48, 0x8d, 0xac, 0x24, 0xa8, 0xfe, 0xff, 0xff};
/* FriendsModule's update: mov [rsp+8],rbx; push rdi; sub rsp,30h; mov rbx,rcx */
static const unsigned char pa_friends_bytes[13] = {0x48, 0x89, 0x5c, 0x24, 0x08, 0x57, 0x48, 0x83,
                                                   0xec, 0x30, 0x48, 0x8b, 0xd9};

typedef const wchar_t *(*PaTextFn)(void *manager, unsigned table, unsigned key);
typedef void (*PaCommitFn)(void *popup, void *list);
typedef void (*PaSelectFn)(void *popup, unsigned char confirmed);
typedef void (*PaFrameFn)(void *popup);
typedef void **(*PaItemFn)(void **out, int *id, void **label);
typedef void (*PaAppendFn)(void *popup, void **collection, void **item);
typedef void **(*PaLocFn)(void **out, const char *table, unsigned key);
typedef void *(*PaMessageFn)(void *parent, void **title, void **body, int type);
typedef void (*PaRebuildFn)(void *popup, unsigned char first);
typedef void (*PaUpdateFn)(void *module);
typedef uint64_t (*PaXuidFn)(void *player);
typedef void (*PaReleaseFn)(void *object);
typedef uint32_t (*PaStateFn)(uint64_t xuid);
typedef uint32_t (*PaActFn)(uint64_t xuid, uint32_t kind, uint32_t reason);
typedef int32_t (*PaResultFn)(uint32_t id, uint32_t *result, uint32_t *state);

static unsigned char *pa_image;
static PaTextFn pa_text_original;
static PaCommitFn pa_commit_original;
static PaSelectFn pa_select_original;
static PaFrameFn pa_frame_original;
static PaUpdateFn pa_friends_original;
static PaItemFn pa_item;
static PaAppendFn pa_append;
static PaLocFn pa_loc;
static PaMessageFn pa_message;
static PaRebuildFn pa_rebuild;
static volatile LONG pa_friends_refresh;

/* ----------------------------------------------------------- texts */

typedef struct {
    const wchar_t *en, *pt;
} PaText;

/* key & 0xffff: 1-8 the items, 9-0x10 the messages (pt-BR with \u escapes: the sources stay ASCII) */
static const PaText pa_texts[] = {
    {0, 0},
    {L"ADD FRIEND", L"ADICIONAR AMIGO"},
    {L"REMOVE FRIEND", L"REMOVER AMIGO"},
    {L"ACCEPT FRIEND REQUEST", L"ACEITAR PEDIDO DE AMIZADE"},
    {L"CANCEL FRIEND REQUEST", L"CANCELAR PEDIDO DE AMIZADE"},
    {L"REPORT PLAYER", L"DENUNCIAR JOGADOR"},
    {L"CHEATING", L"TRAPAÇA"},
    {L"ABUSE", L"ABUSO"},
    {L"OTHER", L"OUTRO"},
    {L"Too many requests. Try again later.", L"Muitas solicitações. Tente novamente mais tarde."},
    {L"Friend request sent.", L"Pedido de amizade enviado."},
    {L"You are now friends.", L"Agora vocês são amigos."},
    {L"Friend removed.", L"Amigo removido."},
    {L"Friend request cancelled.", L"Pedido de amizade cancelado."},
    {L"This player could not be found.", L"Não foi possível encontrar este jogador."},
    {L"You have reached the friends limit.", L"Você atingiu o limite de amigos."},
    {L"Something went wrong. Try again later.", L"Algo deu errado. Tente novamente mais tarde."},
    /* the game's own Storefront report texts (0xa139, 0xf33d), word for word: that table may not be loaded here */
    {L"Your report was submitted.", L"Sua den\u00fancia foi enviada."},
    {L"Your report failed to submit.", L"Falha ao enviar a sua den\u00fancia."},
};
#define PA_TEXT_COUNT (sizeof(pa_texts) / sizeof(pa_texts[0]))
#define PA_KEY_ADD 1u
#define PA_KEY_REMOVE 2u
#define PA_KEY_ACCEPT 3u
#define PA_KEY_CANCEL 4u
#define PA_KEY_REPORT 5u
#define PA_KEY_CHEATING 6u
#define PA_KEY_ABUSE 7u
#define PA_KEY_OTHER 8u
#define PA_KEY_MESSAGES 9u /* PA_RESULT_TRY_LATER .. PA_RESULT_FAILED map to 9 .. 0x10 */
#define PA_KEY_REPORT_SUBMITTED 0x11u
#define PA_KEY_REPORT_FAILED 0x12u

/* Our text for a full 32-bit key, or 0 (not ours: the game's own lookup answers). */
static const wchar_t *pa_text_of(unsigned key) {
    if ((key & 0xffff0000u) != PA_KEY_HIGH) return 0;
    unsigned index = key & 0xffffu;
    if (index == 0 || index >= PA_TEXT_COUNT) return 0;
    return azimuth_lang_portuguese() ? pa_texts[index].pt : pa_texts[index].en;
}

static const wchar_t *pa_text(void *manager, unsigned table, unsigned key) {
    const wchar_t *ours = pa_text_of(key);
    return ours ? ours : pa_text_original(manager, table, key);
}

/* ----------------------------------------------------------- the overlay */

static PaStateFn volatile pa_state_fn;
static PaActFn volatile pa_act_fn;
static PaResultFn volatile pa_result_fn;
static volatile LONG pa_resolved;

static int pa_overlay(void) {
    if (!pa_resolved) {
        if (!fi_launch_overlay_export("fi_overlay_start")) return 0; /* not started (yet) */
        pa_state_fn = (PaStateFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_player_state");
        pa_act_fn = (PaActFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_player_action");
        pa_result_fn = (PaResultFn)(uintptr_t)fi_launch_overlay_export("fi_overlay_player_result");
        InterlockedExchange(&pa_resolved, 1);
    }
    return pa_state_fn && pa_act_fn && pa_result_fn; /* an older overlay: no item at all */
}

/* ----------------------------------------------------------- the popup */

typedef struct {
    void *popup;
    uint64_t xuid;
    uint32_t shown;        /* the state the list was built with */
    uint32_t pending;      /* the overlay's id of an action waiting for its answer */
    uint32_t pending_kind;
    uint32_t local_result; /* an action that could not even be sent: shown on the next frame */
    int reasons;           /* the REPORT reasons sub-list is shown */
    int message_up;        /* our message is over the popup: no rebuild under it (OK closes the popup) */
    uint32_t saved_mask;
    ULONGLONG seen_ms, pending_ms;
} PaPopup;
static PaPopup pa_popups[PA_POPUPS]; /* the game's UI thread only */

static ULONGLONG pa_now(void) { return GetTickCount64(); }

static int pa_is_ours(void *popup) {
    void *vtable = 0;
    return popup && pa_image && memory_read_pointer(popup, &vtable) && vtable == (void *)(pa_image + PA_VTABLE_RVA);
}

static PaPopup *pa_find(void *popup) {
    for (unsigned i = 0; i < PA_POPUPS; ++i) {
        if (pa_popups[i].popup == popup) return &pa_popups[i];
    }
    return 0;
}

/* The popup's slot, a fresh one when it was not drawn for PA_STALE_MS (a new popup at that address). */
static PaPopup *pa_take(void *popup, ULONGLONG now) {
    PaPopup *s = pa_find(popup);
    if (s && now - s->seen_ms <= PA_STALE_MS) return s;
    if (!s) {
        s = &pa_popups[0];
        for (unsigned i = 0; i < PA_POPUPS; ++i) {
            if (!pa_popups[i].popup) {
                s = &pa_popups[i];
                break;
            }
            if (pa_popups[i].seen_ms < s->seen_ms) s = &pa_popups[i];
        }
    }
    memory_copy(s, &(PaPopup){0}, sizeof(*s));
    s->popup = popup;
    s->seen_ms = now;
    return s;
}

static void *pa_virtual(void *object, unsigned slot) {
    void **vtable = 0, *function = 0;
    return object && memory_read_pointer(object, (void **)&vtable) && memory_read_pointer(vtable + slot, &function)
               ? function
               : 0;
}

/* The popup's target XUID (the builder's own read: entry+0x78, its vtable +0x38), 0 when none. */
static uint64_t pa_target(void *popup) {
    void *entry = 0, *player = 0;
    if (!memory_read_pointer((unsigned char *)popup + PA_ENTRY, &entry) ||
        !memory_read_pointer((unsigned char *)entry + PA_ENTRY_PLAYER, &player)) {
        return 0;
    }
    PaXuidFn xuid = (PaXuidFn)(uintptr_t)pa_virtual(player, PA_PLAYER_XUID_SLOT);
    return xuid ? xuid(player) : 0;
}

static void pa_release(void *object) {
    PaReleaseFn release = (PaReleaseFn)(uintptr_t)pa_virtual(object, PA_RELEASE_SLOT);
    if (release) release(object);
}

/* One item, as the builder adds its own: a CLocString, an EnumDataItem, one more reference of the
   collection, then the append (which consumes a reference of each). */
static void pa_add_item(void *popup, void *list, uint32_t id, unsigned key) {
    unsigned char *collection = (unsigned char *)list - PA_LIST_IN_COLLECTION;
    void *label = 0, *item = 0;
    int item_id = (int)id;
    pa_loc(&label, "Lobby", PA_KEY_HIGH | key);
    if (!label) return;
    pa_item(&item, &item_id, &label);
    pa_release(label);
    if (!item) return;
    InterlockedIncrement((volatile LONG *)(collection + PA_REFCOUNT));
    void *held = collection;
    pa_append(popup, &held, &item);
}

static void pa_forget(void *popup) {
    PaPopup *s = pa_find(popup);
    if (s) memory_copy(s, &(PaPopup){0}, sizeof(*s));
}

static void pa_items(void *popup, void *list) {
    uint64_t xuid = pa_overlay() ? pa_target(popup) : 0;
    if (!xuid || xuid == azimuth_account_xuid_value()) { /* no overlay, nobody, or this profile */
        pa_forget(popup);
        return;
    }
    PaPopup *s = pa_take(popup, pa_now());
    if (s->xuid != xuid) {
        void *keep = s->popup;
        memory_copy(s, &(PaPopup){0}, sizeof(*s));
        s->popup = keep;
        s->seen_ms = pa_now();
        s->xuid = xuid;
    }
    uint32_t mask = 0;
    memory_read((unsigned char *)popup + PA_MASK, &mask, sizeof(mask));
    if (s->reasons && mask == PA_REASON_BITS) {
        pa_add_item(popup, list, PA_ID_CHEATING, PA_KEY_CHEATING);
        pa_add_item(popup, list, PA_ID_ABUSE, PA_KEY_ABUSE);
        pa_add_item(popup, list, PA_ID_OTHER, PA_KEY_OTHER);
        return;
    }
    s->reasons = 0;
    uint32_t state = pa_state_fn(xuid);
    s->shown = state;
    if (state == PA_STATE_OWN) return;
    switch (state) {
    case PA_STATE_NONE: pa_add_item(popup, list, PA_ID_ADD, PA_KEY_ADD); break;
    case PA_STATE_FRIEND: pa_add_item(popup, list, PA_ID_REMOVE, PA_KEY_REMOVE); break;
    case PA_STATE_INCOMING: pa_add_item(popup, list, PA_ID_ACCEPT, PA_KEY_ACCEPT); break;
    case PA_STATE_OUTGOING: pa_add_item(popup, list, PA_ID_CANCEL, PA_KEY_CANCEL); break;
    default: break; /* unknown: only REPORT */
    }
    pa_add_item(popup, list, PA_ID_REPORT, PA_KEY_REPORT);
}

/* The list commit, shared by every popup of that base: ours only for PopupPlayerAction. */
static void pa_commit(void *popup, void *list) {
    void *shown = 0;
    if (list && pa_is_ours(popup) && !(memory_read_pointer((unsigned char *)popup + PA_COMMITTED, &shown) && shown == list)) {
        pa_items(popup, list); /* the list it already shows (a re-commit) has ours already */
    }
    pa_commit_original(popup, list);
}

static void pa_send(PaPopup *s, uint32_t kind, uint32_t reason) {
    uint32_t id = pa_act_fn ? pa_act_fn(s->xuid, kind, reason) : 0;
    if (!id) {
        s->local_result = kind == PA_ACT_REPORT ? PA_RESULT_REPORT_FAILED : PA_RESULT_FAILED;
        return;
    }
    s->pending = id;
    s->pending_kind = kind;
    s->pending_ms = pa_now();
}

static void pa_chose(void *popup, uint32_t id) {
    PaPopup *s = pa_find(popup);
    if (!s || !s->xuid || s->pending || s->local_result) return; /* one action at a time */
    uint32_t *mask = (uint32_t *)((unsigned char *)popup + PA_MASK);
    switch (id) {
    case PA_ID_ADD: pa_send(s, PA_ACT_ADD, 0); return;
    case PA_ID_REMOVE: pa_send(s, PA_ACT_REMOVE, 0); return;
    case PA_ID_ACCEPT: pa_send(s, PA_ACT_ACCEPT, 0); return;
    case PA_ID_CANCEL: pa_send(s, PA_ACT_CANCEL, 0); return;
    case PA_ID_REPORT: /* the reasons, in place */
        s->reasons = 1;
        s->saved_mask = *mask;
        *mask = PA_REASON_BITS;
        pa_rebuild(popup, 0);
        return;
    case PA_ID_CHEATING:
    case PA_ID_ABUSE:
    case PA_ID_OTHER:
        if (!s->reasons) return;
        pa_send(s, PA_ACT_REPORT, id == PA_ID_CHEATING ? 1u : id == PA_ID_ABUSE ? 2u : 3u);
        s->reasons = 0;
        *mask = s->saved_mask;
        pa_rebuild(popup, 0); /* the list as it was while the answer comes */
        return;
    default: return;
    }
}

/* The selection: the original first (a native id is dispatched there), then ours. */
static void pa_select(void *popup, unsigned char confirmed) {
    if (!pa_is_ours(popup)) {
        pa_select_original(popup, confirmed);
        return;
    }
    int *selected = (int *)((unsigned char *)popup + PA_SELECTED);
    *selected = 0;
    pa_select_original(popup, confirmed);
    uint32_t id = (uint32_t)*selected;
    if (!id || (id & ~PA_OUR_BITS) || (id & (id - 1))) return; /* none, native, or not one bit */
    *selected = 0;
    pa_chose(popup, id);
}

/* The game's own OK box with our text (the report texts are the game's Storefront words), its result tag 3 as the game
   writes it for its own message here: the parent closes when it is dismissed. */
static void pa_show(void *popup, uint32_t result) {
    unsigned key;
    if (result == PA_RESULT_REPORT_SUBMITTED) {
        key = PA_KEY_HIGH | PA_KEY_REPORT_SUBMITTED;
    } else if (result == PA_RESULT_REPORT_FAILED) {
        key = PA_KEY_HIGH | PA_KEY_REPORT_FAILED;
    } else if (result >= PA_RESULT_TRY_LATER && result <= PA_RESULT_FAILED) {
        key = PA_KEY_HIGH | (PA_KEY_MESSAGES + result - PA_RESULT_TRY_LATER);
    } else {
        return;
    }
    void *title = 0, *body = 0;
    pa_loc(&title, "Lobby", PA_TITLE_KEY);
    pa_loc(&body, "Lobby", key);
    if (title && body) {
        unsigned char *message = pa_message(popup, &title, &body, 0);
        if (message) {
            *(volatile uint64_t *)(message + PA_POPUP_RESULT) = 3;
            PaPopup *s = pa_find(popup);
            if (s) s->message_up = 1;
        }
    }
    if (body) pa_release(body);
    if (title) pa_release(title);
}

/* Slot 153, every frame while the popup is up. */
static void pa_frame(void *popup) {
    pa_frame_original(popup);
    PaPopup *s = pa_find(popup);
    if (!s) return;
    ULONGLONG now = pa_now();
    s->seen_ms = now;
    if (s->local_result) {
        uint32_t result = s->local_result;
        s->local_result = 0;
        pa_show(popup, result);
        return;
    }
    if (s->pending) {
        uint32_t result = 0, state = 0;
        int32_t got = pa_result_fn ? pa_result_fn(s->pending, &result, &state) : -1;
        if (got == 0 && now - s->pending_ms < PA_PENDING_MS) return;
        uint32_t kind = s->pending_kind;
        s->pending = 0;
        if (got != 1) { /* never answered (the launcher went away): say it failed */
            pa_show(popup, kind == PA_ACT_REPORT ? PA_RESULT_REPORT_FAILED : PA_RESULT_FAILED);
            return;
        }
        if (state != PA_STATE_UNKNOWN) s->shown = state;
        if (kind != PA_ACT_REPORT && (result == PA_RESULT_REQUEST_SENT || result == PA_RESULT_FRIENDS ||
                                      result == PA_RESULT_REMOVED || result == PA_RESULT_CANCELLED)) {
            InterlockedExchange(&pa_friends_refresh, 1);
        }
        if (result != PA_RESULT_NONE) pa_show(popup, result);
        else pa_rebuild(popup, 0);
        return;
    }
    if (!s->reasons && !s->message_up && s->xuid && pa_state_fn && pa_state_fn(s->xuid) != s->shown) pa_rebuild(popup, 0);
}

/* FriendsModule's update (its slot 4): the queue flag after a friend change, then the original. */
static void pa_friends_update(void *module) {
    if (module && pa_friends_refresh && InterlockedExchange(&pa_friends_refresh, 0)) {
        *((volatile unsigned char *)module + PA_FRIENDS_QUEUE) = 1;
    }
    pa_friends_original(module);
}

/* ----------------------------------------------------------- install */

static int pa_bytes(unsigned char *image, unsigned rva, const unsigned char *expected, size_t size) {
    unsigned char current[32];
    return size <= sizeof(current) && memory_read(image + rva, current, size) && memory_equal(current, expected, size);
}

static int pa_slot_is(unsigned char *image, unsigned slot_rva, unsigned function_rva) {
    void *value = 0;
    return memory_read(image + slot_rva, &value, sizeof(value)) && value == (void *)(image + function_rva);
}

/* The game build and every byte and slot this file replaces, calls or relies on: 1 when all may be taken. */
static int pa_image_ok(unsigned char *image) {
    IMAGE_DOS_HEADER dos;
    IMAGE_NT_HEADERS64 nt;
    if (!image || !memory_read(image, &dos, sizeof(dos)) || dos.e_magic != IMAGE_DOS_SIGNATURE || dos.e_lfanew < 64 ||
        dos.e_lfanew > 4096 || !memory_read(image + dos.e_lfanew, &nt, sizeof(nt)) || nt.Signature != IMAGE_NT_SIGNATURE ||
        nt.FileHeader.Machine != IMAGE_FILE_MACHINE_AMD64 || nt.FileHeader.TimeDateStamp != GAME_TIMESTAMP ||
        nt.OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC || nt.OptionalHeader.SizeOfImage != GAME_IMAGE_SIZE) {
        return 0;
    }
    return pa_bytes(image, PA_TEXT_RVA, pa_text_bytes, sizeof(pa_text_bytes)) &&
           pa_bytes(image, PA_COMMIT_RVA, pa_commit_bytes, sizeof(pa_commit_bytes)) &&
           pa_bytes(image, PA_SELECT_RVA, pa_select_bytes, sizeof(pa_select_bytes)) &&
           pa_bytes(image, PA_FRAME_RVA, pa_frame_bytes, sizeof(pa_frame_bytes)) &&
           pa_bytes(image, PA_ITEM_RVA, pa_item_bytes, sizeof(pa_item_bytes)) &&
           pa_bytes(image, PA_APPEND_RVA, pa_append_bytes, sizeof(pa_append_bytes)) &&
           pa_bytes(image, PA_LOC_RVA, pa_loc_bytes, sizeof(pa_loc_bytes)) &&
           pa_bytes(image, PA_MESSAGE_RVA, pa_message_bytes, sizeof(pa_message_bytes)) &&
           pa_bytes(image, PA_REBUILD_RVA, pa_rebuild_bytes, sizeof(pa_rebuild_bytes)) &&
           pa_bytes(image, PA_FRIENDS_UPDATE_RVA, pa_friends_bytes, sizeof(pa_friends_bytes)) &&
           pa_slot_is(image, PA_FRAME_SLOT_RVA, PA_FRAME_RVA) && pa_slot_is(image, PA_ANSWER_SLOT_RVA, PA_ANSWER_RVA) &&
           pa_slot_is(image, PA_FRIENDS_SLOT_RVA, PA_FRIENDS_UPDATE_RVA);
}

static int pa_swap_slot(unsigned char *image, unsigned slot_rva, const void *function) {
    void **slot = (void **)(image + slot_rva);
    DWORD old_protection, ignored;
    if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old_protection)) return 0;
    *slot = (void *)(uintptr_t)function;
    VirtualProtect(slot, sizeof(*slot), old_protection, &ignored);
    return 1;
}

/* 1 when everything is in place; 0 when a VirtualAlloc or VirtualProtect failed part way (then the
   items are never added: the list commit goes in last). */
static int pa_install(unsigned char *image) {
    if (!pa_image_ok(image)) return 0;
    unsigned char *code = VirtualAlloc(0, 3 * 32, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!code) return 0;
    pa_image = image;
    pa_item = (PaItemFn)(uintptr_t)(image + PA_ITEM_RVA);
    pa_append = (PaAppendFn)(uintptr_t)(image + PA_APPEND_RVA);
    pa_loc = (PaLocFn)(uintptr_t)(image + PA_LOC_RVA);
    pa_message = (PaMessageFn)(uintptr_t)(image + PA_MESSAGE_RVA);
    pa_rebuild = (PaRebuildFn)(uintptr_t)(image + PA_REBUILD_RVA);
    pa_frame_original = (PaFrameFn)(uintptr_t)(image + PA_FRAME_RVA);
    pa_friends_original = (PaUpdateFn)(uintptr_t)(image + PA_FRIENDS_UPDATE_RVA);
    /* each trampoline is named before its jump goes in: the game may call the detour at once */
    pa_text_original = (PaTextFn)(uintptr_t)code;
    pa_select_original = (PaSelectFn)(uintptr_t)(code + 32);
    pa_commit_original = (PaCommitFn)(uintptr_t)(code + 64);
    if (!memory_detour14(image + PA_TEXT_RVA, pa_text_bytes, PA_MOVED, (const void *)(uintptr_t)pa_text, code)) return 0;
    if (!pa_swap_slot(image, PA_FRAME_SLOT_RVA, (const void *)(uintptr_t)pa_frame)) return 0;
    pa_swap_slot(image, PA_FRIENDS_SLOT_RVA, (const void *)(uintptr_t)pa_friends_update); /* optional: the game's own refresh */
    if (!memory_detour14(image + PA_SELECT_RVA, pa_select_bytes, PA_MOVED, (const void *)(uintptr_t)pa_select, code + 32)) {
        return 0;
    }
    /* last: no item is ever added before everything that handles it is in place */
    return memory_detour14(image + PA_COMMIT_RVA, pa_commit_bytes, PA_MOVED, (const void *)(uintptr_t)pa_commit, code + 64) != 0;
}

void fi_player_actions_apply(unsigned char *game_image) {
    static long installed;
    if (InterlockedCompareExchange(&installed, 1, 0) != 0) return;
    if (!pa_image_ok(game_image)) {
        log_append("PLAYER ACTIONS: the game's player menu is not the supported build: no friend or report items.\r\n");
        return;
    }
    log_append(pa_install(game_image)
                   ? "PLAYER ACTIONS: the game's PLAYER ACTIONS offers the friend items and REPORT PLAYER (through the launcher).\r\n"
                   : "PLAYER ACTIONS: installing failed part way (VirtualAlloc or VirtualProtect); no item is added.\r\n");
}
