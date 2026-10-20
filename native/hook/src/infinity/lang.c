/* ForzaInfinity hook, GPL-3.0-or-later (see LICENSE).
   Notice translations selected through read-only access to the game's string-table manager.
   Unknown text is unchanged. Before the manager exists, auto uses English. */
#include "infinity.h"
#include "../core/logging.h"
#include "../core/support.h"
typedef struct {
    const wchar_t *english, *portuguese;
} LangText;
typedef struct {
    const char *english, *portuguese;
} LangText8;
static volatile LONG lang_portuguese;
static const LangText lang_texts[] = {
    {L"The online profile's save on this PC differed from the server's, so the server's was put in its place. If the game shows other progress, restart it.",
     L"O save do perfil online neste PC era diferente do servidor e foi substituído pelo save do servidor. Se o jogo mostrar outro progresso, reinicie-o."},
    {L"ExtraOptions features are now separate. Install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.",
     L"Os recursos do ExtraOptions agora são separados. Instale o ExtraOptions v0.2.98 e reinstale o hook Infinity pelo launcher."},
    {L"ExtraOptions features are now separate. Update Azimuth to 0.9.10, install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.",
     L"Os recursos do ExtraOptions agora são separados. Atualize o Azimuth para 0.9.10, instale o ExtraOptions v0.2.98 e reinstale o hook Infinity pelo launcher."},
    {L"ExtraOptions: not looked at yet.",
     L"ExtraOptions: ainda não verificado."},
    {L"ExtraOptions v0.2.98 found: headlight and roof sync on.",
     L"ExtraOptions v0.2.98 encontrado: sincronização de faróis e teto ligada."},
    {L"ExtraOptions v0.2.98 found; headlight and roof sync is off in Infinity.cfg (CarState=off).",
     L"ExtraOptions v0.2.98 encontrado; a sincronização de faróis e teto está desligada no Infinity.cfg (CarState=off)."},
    {L"ExtraOptions v0.2.98 is installed but not active (its FH2_ExtraOptions.log says why): headlight and roof sync off.",
     L"O ExtraOptions v0.2.98 está instalado, mas não ativo (o FH2_ExtraOptions.log dele diz por quê): sincronização de faróis e teto desligada."},
    {L"ExtraOptions found, not v0.2.98 (headlight and roof sync needs v0.2.98): ",
     L"ExtraOptions encontrado, mas não é a v0.2.98 (a sincronização de faróis e teto precisa da v0.2.98): "},
    {L"A winmm.dll that is not ExtraOptions is next to azimuth.exe: headlight and roof sync off.",
     L"Há um winmm.dll que não é o ExtraOptions ao lado do azimuth.exe: sincronização de faróis e teto desligada."},
    {L"ExtraOptions is installed but did not load (on Steam Deck or Linux the game needs WINEDLLOVERRIDES=\"winmm.dll=n,b\"): headlight and roof sync off.",
     L"O ExtraOptions está instalado, mas não carregou (no Steam Deck ou no Linux o jogo precisa de WINEDLLOVERRIDES=\"winmm.dll=n,b\"): sincronização de faróis e teto desligada."},
    {L"ExtraOptions not installed: everything works except headlight and roof sync.",
     L"ExtraOptions não instalado: tudo funciona, exceto a sincronização de faróis e teto."},
    {L"The world could not be started (infinity.exe relay did not run).",
     L"Não deu para abrir o mundo (o infinity.exe relay não rodou)."},
    {L"OK",
     L"OK"},
    {L"ForzaInfinity ",
     L"ForzaInfinity "},
    {L"\n\nFREE ROAM (worlds over the Internet)",
     L"\n\nPASSEIO LIVRE (mundos pela internet)"},
    {L"\nNo answer from the STUN servers: this network seems to block UDP. Playing with players outside your network will not work until it allows it (a firewall, or a school or work network).",
     L"\nOs servidores STUN não responderam: esta rede parece bloquear UDP. Jogar com quem está fora da sua rede não vai funcionar enquanto ela não permitir (um firewall, ou uma rede de escola ou de trabalho)."},
    {L"\nYour address on the Internet: ",
     L"\nSeu endereço na internet: "},
    {L"\nIPv6: yes.",
     L"\nIPv6: sim."},
    {L"\nIPv6: no.",
     L"\nIPv6: não."},
    {L"\nNAT: moderate (cone). You can join most players' worlds and most can join yours, with no port to open.",
     L"\nNAT: moderado (cone). Você entra no mundo da maioria dos jogadores e a maioria entra no seu, sem abrir porta nenhuma."},
    {L"\nNAT: strict (symmetric). You can join worlds whose host has a moderate or open NAT, but two strict NATs cannot meet. Fixes: turn UPnP on in the router, ask your operator for a public IP (CGNAT), or forward UDP 5101 to this PC when hosting.",
     L"\nNAT: restrito (simétrico). Você entra em mundos cujo host tem NAT moderado ou aberto, mas dois NATs restritos não se encontram. Soluções: ligar o UPnP no roteador, pedir IP público à operadora (CGNAT) ou, ao hospedar, encaminhar a porta UDP 5101 para este PC."},
    {L"\nNAT: not measured (only one STUN server answered).",
     L"\nNAT: não medido (só um servidor STUN respondeu)."},
    {L"\n\nYOUR WORLD",
     L"\n\nSEU MUNDO"},
    {L"\nPlayers reach it at ",
     L"\nOs jogadores chegam nele por "},
    {L"\nUPnP: still asking the router.",
     L"\nUPnP: ainda perguntando ao roteador."},
    {L"\nUPnP: not available on this router (players still get in by hole punching).",
     L"\nUPnP: não disponível neste roteador (os jogadores entram mesmo assim, por hole punching)."},
    {L"\nUPnP: the router refused (",
     L"\nUPnP: o roteador recusou ("},
    {L"); players still get in by hole punching.",
     L"); os jogadores entram mesmo assim, por hole punching."},
    {L"\nUPnP: the router forwarded the world's port.",
     L"\nUPnP: o roteador abriu a porta do mundo."},
    {L"\nYour router is behind another router or the operator's CGNAT: players outside may not get in. Put the operator's modem in bridge mode, or ask for a public IP.",
     L"\nSeu roteador está atrás de outro roteador ou do CGNAT da operadora: jogadores de fora podem não conseguir entrar. Coloque o modem da operadora em modo bridge ou peça um IP público."},
    {L"\nGuide: github.com/anarqz/ForzaInfinity/blob/main/docs/FREEROAM.md",
     L"\nGuia: github.com/anarqz/ForzaInfinity/blob/main/docs/FREEROAM.md"},
    {L"Close world",
     L"Encerrar mundo"},
    {L"The car database changed after the game started. Restart the game to play online.",
     L"O banco de carros mudou depois que o jogo abriu. Reinicie o jogo para jogar online."},
    {L"Hosting a world needs your online profile.",
     L"Para hospedar um mundo, use o seu perfil online."},
    {L"dedicated_server.exe is missing from the game's data\\bin folder. Reinstall ForzaInfinity.",
     L"O dedicated_server.exe não está na pasta data\\bin do jogo. Reinstale o ForzaInfinity."},
    {L"The world could not be started (job object).",
     L"Não deu para abrir o mundo (job object)."},
    {L"The world could not be started (dedicated_server.exe did not run).",
     L"Não deu para abrir o mundo (o dedicated_server.exe não rodou)."},
    {L"This server does not take hosted worlds yet.",
     L"Este servidor ainda não aceita mundos hospedados."},
    {L"The server did not take the world. Try again in a moment.",
     L"O servidor não aceitou o mundo. Tente de novo daqui a pouco."},
    {L"\n",
     L"\n"},
};
static const LangText8 lang_texts8[] = {
    {"The online profile's save on this PC differed from the server's, so the server's was put in its place. If the game shows other progress, restart it.",
     "O save do perfil online neste PC era diferente do servidor e foi substituído pelo save do servidor. Se o jogo mostrar outro progresso, reinicie-o."},
    {"ExtraOptions features are now separate. Install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.",
     "Os recursos do ExtraOptions agora são separados. Instale o ExtraOptions v0.2.98 e reinstale o hook Infinity pelo launcher."},
    {"ExtraOptions features are now separate. Update Azimuth to 0.9.10, install ExtraOptions v0.2.98, then reinstall the Infinity hook from the launcher.",
     "Os recursos do ExtraOptions agora são separados. Atualize o Azimuth para 0.9.10, instale o ExtraOptions v0.2.98 e reinstale o hook Infinity pelo launcher."},
    {"ForzaInfinity ",
     "ForzaInfinity "},
    {" is installed (",
     " foi instalado ("},
    {" before). Restart the game to use it; your server and settings stay as they are.",
     " antes). Reinicie o jogo para usar; seu servidor e suas configurações continuam iguais."},
    {" is out, but it could not be installed by itself (",
     " saiu, mas não foi possível instalar sozinho ("},
    {").\n\nClose the game and run ForzaInfinity-Update in the Azimuth folder (next to azimuth.exe).",
     ").\n\nFeche o jogo e rode o ForzaInfinity-Update na pasta do Azimuth (ao lado do azimuth.exe)."},
    {" is out (this is ",
     " saiu (esta é a "},
    {").\n\nClose the game and run ForzaInfinity-Update in the Azimuth folder (next to azimuth.exe): it installs the new version and keeps your server and settings.",
     ").\n\nFeche o jogo e rode o ForzaInfinity-Update na pasta do Azimuth (ao lado do azimuth.exe): ele instala a versão nova e mantém seu servidor e suas configurações."},
    {"Downloading ForzaInfinity ",
     "Baixando o ForzaInfinity "},
    {"...",
     "..."},
    {"GitHub did not answer",
     "o GitHub não respondeu"},
    {"the release's SHA-256 is not readable",
     "não deu para ler o SHA-256 da release"},
    {"out of memory",
     "sem memória"},
    {"the download failed",
     "o download falhou"},
    {"the download does not match the release's SHA-256",
     "o download não confere com o SHA-256 da release"},
    {"the download is not the version the server named",
     "o download não é a versão que o servidor indicou"},
    {"this file's place not found",
     "não achei onde este arquivo está"},
    {"this file is not named azimuth-tls.dll",
     "este arquivo não se chama azimuth-tls.dll"},
    {"could not write in data\\bin",
     "não foi possível gravar em data\\bin"},
    {"could not move the current file aside",
     "não foi possível tirar o arquivo atual do lugar"},
    {"could not put the new file in place",
     "não foi possível colocar o arquivo novo no lugar"},
    {"This world does not run events: its host turned races off or has not updated ForzaInfinity yet. Play them in HORIZON SOLO or in another world.",
     "Este mundo não conduz eventos: o host desligou as corridas ou ainda não atualizou o ForzaInfinity. Jogue-os no HORIZON SOLO ou em outro mundo."},
    {"Cloud sync stopped: the server did not accept some of this session's progress. Nothing from now on is saved online. Restart the game to play on from your cloud save.",
     "Sincronização com a nuvem interrompida: o servidor não aceitou parte do progresso desta sessão. Nada a partir de agora é salvo online. Reinicie o jogo para continuar a partir do seu save da nuvem."},
};
/* Infinity.cfg [hook] Language: en or pt-BR fixes the language; auto (the default) follows the
   game's, read from its string-table manager when a text is needed (OD-73: the hook no longer
   edits the Message Center table it used to tell the language from). SAURON's RE: game+0x36629a0
   holds the manager (0 until the game builds it); manager+0x28 is the active language index
   (0..16, FUN_141245cf0 sets it from the Windows locale or azimuth.ini's language=, and it can
   change later), manager+0x38 + index*0xe0 its stringtables folder code ("BR" for index 11). */
#define LANG_MANAGER_RVA 0x36629a0u
#define LANG_INDEX_OFFSET 0x28u
#define LANG_CODES_OFFSET 0x38u
#define LANG_CODE_STRIDE 0xe0u
#define LANG_COUNT 17
static int lang_forced = -1;
static unsigned char *lang_game;
static volatile LONG lang_logged = -1;
void azimuth_lang_force(int language) {
    lang_forced = language == FI_LANG_PT_BR ? 1 : language == FI_LANG_EN ? 0 : -1;
    if (lang_forced >= 0) {
        InterlockedExchange(&lang_portuguese, lang_forced);
    }
}
void azimuth_lang_apply(unsigned char *game_image) {
    lang_game = game_image;
}
/* The game's language now: 1 Portuguese (Brazil), 0 anything else, -1 not known yet. */
static int lang_game_portuguese(void) {
    unsigned char *manager = 0;
    int index = -1;
    char code[2];
    if (!lang_game || !memory_read_pointer(lang_game + LANG_MANAGER_RVA, (void **)&manager) || !manager ||
        !memory_read(manager + LANG_INDEX_OFFSET, &index, sizeof(index)) || index < 0 || index >= LANG_COUNT ||
        !memory_read(manager + LANG_CODES_OFFSET + (size_t)index * LANG_CODE_STRIDE, code, sizeof(code))) {
        return -1;
    }
    int portuguese = (code[0] == 'B' || code[0] == 'b') && (code[1] == 'R' || code[1] == 'r');
    /* once per language seen, to confirm the RE on a live game */
    if (InterlockedExchange(&lang_logged, index) != index) {
        log_append_value("LANG: the game's language index ", index);
        log_append(portuguese ? "LANG: that is Portuguese (Brazil).\r\n" : "LANG: that is not Portuguese.\r\n");
    }
    return portuguese;
}
int azimuth_lang_portuguese(void) {
    if (lang_forced < 0) {
        int game = lang_game_portuguese();
        if (game >= 0) {
            InterlockedExchange(&lang_portuguese, game);
        }
    }
    return lang_portuguese != 0;
}
static int lang_equal(const wchar_t *a, const wchar_t *b) {
    while (*a && *a == *b) {
        ++a;
        ++b;
    }
    return *a == *b;
}
const wchar_t *azimuth_tr(const wchar_t *english) {
    if (!english || !azimuth_lang_portuguese()) {
        return english;
    }
    for (size_t i = 0; i < ARRAY_COUNT(lang_texts); ++i) {
        if (lang_equal(english, lang_texts[i].english)) {
            return lang_texts[i].portuguese;
        }
    }
    return english;
}
const char *azimuth_tr8(const char *english) {
    if (!english || !azimuth_lang_portuguese()) {
        return english;
    }
    size_t n = text_length(english);
    for (size_t i = 0; i < ARRAY_COUNT(lang_texts8); ++i) {
        if (text_length(lang_texts8[i].english) == n && memory_equal(english, lang_texts8[i].english, n)) {
            return lang_texts8[i].portuguese;
        }
    }
    return english;
}
