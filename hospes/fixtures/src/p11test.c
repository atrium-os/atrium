/* S5 fixture: a small PKCS#11 module shaped like a token vendor's, for sigillum-p11's
 * check (hospes/sigillum-p11-check). It reaches its "token" only through PC/SC, as the
 * InnaIT module does: SCardEstablishContext, SCardListReaders (auto-allocated),
 * SCardConnect, SCardTransmit. Its card (the check's) answers a few APDUs:
 *   VERIFY       00 20 00 80 [pin]   9000, 63Cx (x tries left), 6983 (blocked)
 *   READ BINARY  00 B0 00 00 00      the certificate
 *   SIGN         80 2A mm pp [data]  a signature (mm: 1 ECDSA, 2 RSA PKCS#1, 3 PSS;
 *                                    pp: PSS hash and salt as the module saw them)
 *   FORBIDDEN    80 FF ii 00         a call outside Sigillum's allow-list (ii: index)
 * Every CK_FUNCTION_LIST entry outside the allow-list reports itself with FORBIDDEN, so
 * the check can prove the host never calls one. Built -nostdlib: only SCard* and a few
 * libc names are imported, all unversioned. */
typedef unsigned long UL;
typedef unsigned char U8;
typedef long LONG;

/* pcsc-lite (LP64) */
extern LONG SCardEstablishContext(UL, const void *, const void *, LONG *);
extern LONG SCardListReaders(LONG, const char *, char *, UL *);
extern LONG SCardFreeMemory(LONG, const void *);
extern LONG SCardConnect(LONG, const char *, UL, UL, LONG *, UL *);
extern LONG SCardTransmit(LONG, const void *, const U8 *, UL, void *, U8 *, UL *);
extern LONG SCardDisconnect(LONG, UL);
extern LONG SCardReleaseContext(LONG);
struct io_request { UL proto, len; };
static const struct io_request T1 = { 2, 16 };
extern void *memcpy(void *, const void *, unsigned long);
extern void *memset(void *, int, unsigned long);

#define EXPORT __attribute__((visibility("default")))
#define CKR_OK 0
#define CKR_GENERAL_ERROR 5
#define CKR_ARGUMENTS_BAD 7
#define CKR_FUNCTION_NOT_SUPPORTED 0x54
#define CKR_KEY_HANDLE_INVALID 0x60
#define CKR_MECHANISM_INVALID 0x70
#define CKR_MECHANISM_PARAM_INVALID 0x71
#define CKR_OBJECT_HANDLE_INVALID 0x82
#define CKR_PIN_INCORRECT 0xA0
#define CKR_PIN_LOCKED 0xA4
#define CKR_USER_NOT_LOGGED_IN 0x101
#define CKR_BUFFER_TOO_SMALL 0x150
#define CKR_CRYPTOKI_NOT_INITIALIZED 0x190
#define CKR_ATTRIBUTE_TYPE_INVALID 0x12
#define UNAVAILABLE ((UL)-1)

struct attr { UL type; void *value; UL len; };
struct mech { UL type; const void *param; UL len; };
struct pss { UL hash, mgf, salt; };
struct token_info {
    U8 label[32], manufacturer[32], model[16], serial[16];
    UL flags, counts[10];
    U8 versions[4], utc[16];
};
struct function_list { U8 version[2]; void *f[68]; };

static LONG ctx, card;
static int ready, logged_in, find_class, find_pos, sign_mech;
static U8 find_id[64];
static UL find_id_len;
static U8 pss_byte;

static LONG apdu(const U8 *a, UL n, U8 *out, UL *outn) {
    return SCardTransmit(card, &T1, a, n, 0, out, outn);
}
static unsigned sw(const U8 *r, UL n) { return n >= 2 ? (unsigned)r[n - 2] << 8 | r[n - 1] : 0; }

static UL forbidden_call(U8 index) {
    U8 a[4] = { 0x80, 0xFF, index, 0 }, r[2];
    UL n = sizeof r;
    if (ready) apdu(a, 4, r, &n);
    return CKR_FUNCTION_NOT_SUPPORTED;
}
#define FORBID(i) static UL forbidden_##i(void) { return forbidden_call(i); }
FORBID(2) FORBID(3) FORBID(5) FORBID(7) FORBID(8) FORBID(9) FORBID(10) FORBID(11) FORBID(14) FORBID(15)
FORBID(16) FORBID(17) FORBID(20) FORBID(21) FORBID(22) FORBID(23) FORBID(25) FORBID(29) FORBID(30) FORBID(31)
FORBID(32) FORBID(33) FORBID(34) FORBID(35) FORBID(36) FORBID(37) FORBID(38) FORBID(39) FORBID(40) FORBID(41)
FORBID(44) FORBID(45) FORBID(46) FORBID(47) FORBID(48) FORBID(49) FORBID(50) FORBID(51) FORBID(52) FORBID(53)
FORBID(54) FORBID(55) FORBID(56) FORBID(57) FORBID(58) FORBID(59) FORBID(60) FORBID(61) FORBID(62) FORBID(63)
FORBID(64) FORBID(65) FORBID(66) FORBID(67)

static UL C_Initialize(void *args) {
    (void)args;
    if (SCardEstablishContext(2, 0, 0, &ctx)) return CKR_GENERAL_ERROR;
    char *readers = 0;
    UL n = (UL)-1;
    if (SCardListReaders(ctx, 0, (char *)&readers, &n) || !readers) return CKR_GENERAL_ERROR;
    UL proto = 0;
    LONG r = SCardConnect(ctx, readers, 2, 3, &card, &proto);
    SCardFreeMemory(ctx, readers);
    if (r || proto != 2) return CKR_GENERAL_ERROR;
    ready = 1;
    return CKR_OK;
}
static UL C_Finalize(void *r) {
    (void)r;
    SCardDisconnect(card, 0);
    SCardReleaseContext(ctx);
    ready = 0;
    return CKR_OK;
}
static UL C_GetSlotList(U8 present, UL *list, UL *count) {
    (void)present;
    if (!ready) return CKR_CRYPTOKI_NOT_INITIALIZED;
    if (list) {
        if (*count < 1) { *count = 1; return CKR_BUFFER_TOO_SMALL; }
        list[0] = 1;
    }
    *count = 1;
    return CKR_OK;
}
static void pad(U8 *d, const char *s, int n) {
    int i = 0;
    for (; s[i] && i < n; i++) d[i] = (U8)s[i];
    for (; i < n; i++) d[i] = ' ';
}
static UL C_GetTokenInfo(UL slot, struct token_info *t) {
    if (slot != 1) return CKR_ARGUMENTS_BAD;
    memset(t, 0, sizeof *t);
    pad(t->label, "Sigillum test token", 32);
    pad(t->manufacturer, "Atrium", 32);
    pad(t->model, "p11test", 16);
    pad(t->serial, "0001", 16);
    t->flags = 0x4 | 0x400; /* LOGIN_REQUIRED, TOKEN_INITIALIZED */
    /* VERIFY without data: how many tries are left. */
    U8 a[4] = { 0, 0x20, 0, 0x80 }, r[2];
    UL n = sizeof r;
    if (apdu(a, 4, r, &n) == 0) {
        unsigned s = sw(r, n);
        if (s == 0x63C1) t->flags |= 0x20000;       /* FINAL_TRY */
        else if (s == 0x6983) t->flags |= 0x40000;  /* LOCKED */
    }
    return CKR_OK;
}
static UL C_OpenSession(UL slot, UL flags, void *app, void *notify, UL *h) {
    (void)flags; (void)app; (void)notify;
    if (slot != 1) return CKR_ARGUMENTS_BAD;
    *h = 7;
    return CKR_OK;
}
static UL C_CloseSession(UL h) { (void)h; return CKR_OK; }
static UL C_Login(UL h, UL user, const U8 *pin, UL len) {
    (void)h;
    if (user != 1 || len == 0 || len > 32) return CKR_ARGUMENTS_BAD;
    U8 a[5 + 32], r[2];
    a[0] = 0; a[1] = 0x20; a[2] = 0; a[3] = 0x80; a[4] = (U8)len;
    memcpy(a + 5, pin, len);
    UL n = sizeof r;
    LONG rv = apdu(a, 5 + len, r, &n);
    memset(a, 0, sizeof a);
    if (rv) return CKR_GENERAL_ERROR;
    unsigned s = sw(r, n);
    if (s == 0x9000) { logged_in = 1; return CKR_OK; }
    if (s == 0x6983) return CKR_PIN_LOCKED;
    return CKR_PIN_INCORRECT;
}
static UL C_Logout(UL h) { (void)h; logged_in = 0; return CKR_OK; }

/* Objects: 1 the certificate, 2 its private key (both visible without login). */
static const U8 ID[] = { 0x01, 0x02 };
static const U8 P256[] = { 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07 };
static UL put(struct attr *a, const void *v, UL n) {
    if (!a->value) { a->len = n; return CKR_OK; }
    if (a->len < n) { a->len = UNAVAILABLE; return CKR_BUFFER_TOO_SMALL; }
    memcpy(a->value, v, n);
    a->len = n;
    return CKR_OK;
}
static UL C_GetAttributeValue(UL h, UL obj, struct attr *t, UL n) {
    (void)h;
    if (obj != 1 && obj != 2) return CKR_OBJECT_HANDLE_INVALID;
    UL rv = CKR_OK;
    for (UL i = 0; i < n; i++) {
        struct attr *a = &t[i];
        UL cls = obj == 1 ? 1 : 3, kt = 3;
        U8 cert[260];
        UL cn = sizeof cert;
        switch (a->type) {
        case 0x000: rv |= put(a, &cls, 8); break;
        case 0x102: rv |= put(a, ID, sizeof ID); break;
        case 0x003: rv |= put(a, obj == 1 ? "Signing certificate" : "Signing key", obj == 1 ? 19 : 11); break;
        case 0x011:
            if (obj != 1) { a->len = UNAVAILABLE; rv = CKR_ATTRIBUTE_TYPE_INVALID; break; }
            {
                U8 rb[5] = { 0, 0xB0, 0, 0, 0 };
                if (apdu(rb, 5, cert, &cn) || sw(cert, cn) != 0x9000) return CKR_GENERAL_ERROR;
                rv |= put(a, cert, cn - 2);
            }
            break;
        case 0x100:
            if (obj != 2) { a->len = UNAVAILABLE; rv = CKR_ATTRIBUTE_TYPE_INVALID; break; }
            rv |= put(a, &kt, 8);
            break;
        case 0x180:
            if (obj != 2) { a->len = UNAVAILABLE; rv = CKR_ATTRIBUTE_TYPE_INVALID; break; }
            rv |= put(a, P256, sizeof P256);
            break;
        default: a->len = UNAVAILABLE; rv = CKR_ATTRIBUTE_TYPE_INVALID;
        }
    }
    return rv;
}
static UL C_FindObjectsInit(UL h, const struct attr *t, UL n) {
    (void)h;
    find_class = -1; find_id_len = 0; find_pos = 0;
    for (UL i = 0; i < n; i++) {
        if (t[i].type == 0 && t[i].len == 8) find_class = (int)*(const UL *)t[i].value;
        if (t[i].type == 0x102 && t[i].len <= sizeof find_id) { memcpy(find_id, t[i].value, t[i].len); find_id_len = t[i].len; if (!find_id_len) find_id_len = (UL)-1; }
    }
    return CKR_OK;
}
static int id_matches(void) {
    if (!find_id_len) return 1;
    if (find_id_len != sizeof ID) return 0;
    for (UL i = 0; i < sizeof ID; i++) if (find_id[i] != ID[i]) return 0;
    return 1;
}
static UL C_FindObjects(UL h, UL *out, UL max, UL *n) {
    (void)h;
    *n = 0;
    if (find_pos || !max || !id_matches()) return CKR_OK;
    if (find_class == 1 || find_class == -1) out[(*n)++] = 1;
    if ((find_class == 3 || find_class == -1) && *n < max) out[(*n)++] = 2;
    find_pos = 1;
    return CKR_OK;
}
static UL C_FindObjectsFinal(UL h) { (void)h; return CKR_OK; }
static UL C_SignInit(UL h, const struct mech *m, UL key) {
    (void)h;
    if (!logged_in) return CKR_USER_NOT_LOGGED_IN;
    if (key != 2) return CKR_KEY_HANDLE_INVALID;
    switch (m->type) {
    case 0x1041: sign_mech = 1; pss_byte = 0; return CKR_OK;
    case 0x0001: sign_mech = 2; pss_byte = 0; return CKR_OK;
    case 0x000D: {
        if (m->len != sizeof(struct pss)) return CKR_MECHANISM_PARAM_INVALID;
        const struct pss *p = m->param;
        sign_mech = 3;
        pss_byte = (U8)((p->hash & 0xF0) | (p->mgf & 0x0F)); /* 0x52 for SHA-256/MGF1-SHA256 */
        if (p->salt != 32 && p->salt != 48 && p->salt != 64) return CKR_MECHANISM_PARAM_INVALID;
        return CKR_OK;
    }
    }
    return CKR_MECHANISM_INVALID;
}
static UL C_Sign(UL h, const U8 *data, UL len, U8 *sig, UL *siglen) {
    (void)h;
    if (!sign_mech) return CKR_GENERAL_ERROR;
    if (len > 200) return CKR_ARGUMENTS_BAD;
    U8 a[5 + 200 + 1], r[520];
    a[0] = 0x80; a[1] = 0x2A; a[2] = (U8)sign_mech; a[3] = pss_byte; a[4] = (U8)len;
    memcpy(a + 5, data, len);
    a[5 + len] = 0;
    UL n = sizeof r;
    if (apdu(a, 6 + len, r, &n) || sw(r, n) != 0x9000) return CKR_GENERAL_ERROR;
    n -= 2;
    if (!sig) { *siglen = n; return CKR_OK; }
    if (*siglen < n) { *siglen = n; return CKR_BUFFER_TOO_SMALL; }
    memcpy(sig, r, n);
    *siglen = n;
    sign_mech = 0;
    return CKR_OK;
}

static struct function_list list;
EXPORT UL C_GetFunctionList(struct function_list **out) {
    list.version[0] = 2; list.version[1] = 40;
    void *f[68] = {
        C_Initialize, C_Finalize, forbidden_2, forbidden_3, C_GetSlotList, forbidden_5, C_GetTokenInfo, forbidden_7,
        forbidden_8, forbidden_9, forbidden_10, forbidden_11, C_OpenSession, C_CloseSession, forbidden_14, forbidden_15,
        forbidden_16, forbidden_17, C_Login, C_Logout, forbidden_20, forbidden_21, forbidden_22, forbidden_23,
        C_GetAttributeValue, forbidden_25, C_FindObjectsInit, C_FindObjects, C_FindObjectsFinal, forbidden_29, forbidden_30, forbidden_31,
        forbidden_32, forbidden_33, forbidden_34, forbidden_35, forbidden_36, forbidden_37, forbidden_38, forbidden_39,
        forbidden_40, forbidden_41, C_SignInit, C_Sign, forbidden_44, forbidden_45, forbidden_46, forbidden_47,
        forbidden_48, forbidden_49, forbidden_50, forbidden_51, forbidden_52, forbidden_53, forbidden_54, forbidden_55,
        forbidden_56, forbidden_57, forbidden_58, forbidden_59, forbidden_60, forbidden_61, forbidden_62, forbidden_63,
        forbidden_64, forbidden_65, forbidden_66, forbidden_67,
    };
    memcpy(list.f, f, sizeof f);
    *out = &list;
    return CKR_OK;
}
/* A vendor administrative export, like C_InnaIT*: never resolved by the host. */
EXPORT UL C_TestZeroize(void) { return forbidden_call(0xEE); }
