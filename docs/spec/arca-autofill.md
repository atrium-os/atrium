# Autofill: credentials filled without the app ever holding them

Status: accepted, 2026-11-06 (Pergola thread; the user agreed to all §10 recommendations). It extends [arca.md](arca.md) §8.3, which
gives the rules for web passwords in one paragraph, into one mechanism for every app. It
also uses the press tokens of [tabula.md](tabula.md) §2 and the embeds of
[limen.md](limen.md).

## 0. The idea in one paragraph

A credential field is **not the app's field**. It is a small surface that Arca owns,
embedded in the app's window through Limen, the same way Vestibulum's sign-in prompt is
embedded (vestibulum.md §4.3). Typing into it, filling it, generating a password in it and
saving from it all happen inside Arca's process. The app gets a **handle**, never the
value. It submits that handle through a courier, which puts the secret only into a TLS
connection to the item's recipient (arca.md §4.3, §5). Every hard problem of autofill
becomes "who owns the pixels and the keys of that field", and frescod already answers that.

## 1. What goes wrong elsewhere, and the rule that stops each one here

These are the known ways autofill leaks, from browser and mobile password managers. Each
row is a rule this design must keep.

| attack | how it works elsewhere | the rule here |
|---|---|---|
| **silent fill and harvest** | a page carries a hidden login form; the manager fills it on load and script reads it | Nothing is filled without a press on Arca's own chip (§3). The app can't read the field afterwards either (§0) |
| **hidden or tiny field** | an off-screen, 1 px or `opacity: 0` field takes the fill | frescod fills only a field that is on screen, unobscured, at full opacity and at least a minimum size (§4) |
| **clickjacking the chip** | the page draws over the fill prompt, or moves something under the pointer just before the click | The app may not draw above a credential slot, and a press counts only if the slot was unobscured and still for 500 ms (§4) |
| **phishing / lookalike origin** | `paypa1.com` gets the fill, or the user types the password into it | Exact recipient match (arca.md §3); confusable hosts refused; the chip shows the real recipient, drawn by Arca (§5) |
| **subdomain and iframe sharing** | `evil.example.com` or an ad iframe on the page gets `example.com`'s password | The recipient is the origin the courier reached for that frame. No registrable-domain matching; any other origin needs a link the user made (§5) |
| **app reads the value** | a native app's text field holds the password in its own memory | The value lives only in Arca and the courier (§0). App-held is the stated exception, and it needs a grant (§7) |
| **fake field, typed by hand** | an app draws a lookalike field and the user types the password themselves | Not preventable in general. A real credential field has system chrome no app can draw (§5), and Arca never fills an unmarked field, so its not offering to fill is the warning |
| **synthetic input** | a script sends the keystrokes or click that confirms the fill | Synthetic input gets no press token (tabula.md §2.1), and the token is spent by Arca, not the app (§3) |
| **leak via clipboard** | "copy password" leaves it on the clipboard for everyone | Copy needs a Vestibulum check and is a sensitive Tabula entry, text only, cleared after 60 s (tabula.md §3.1) |
| **echo and autocomplete in the app** | the app's own history or undo stack keeps the password | The app never had it (§0) |
| **fill on a locked or unattended screen** | the screen is locked but a page still fills | Passwords are while-unlocked (arca.md §6.2); optionally verify-each-use per item |

## 2. Marking a credential field

The app says what a field is for:

| purpose | web | Pergola |
|---|---|---|
| `username` | `autocomplete=username`, or the text input before a password | `CredentialField::username()` |
| `current-password` | `type=password`, `autocomplete=current-password` | `CredentialField::password()` |
| `new-password` | `autocomplete=new-password` | `CredentialField::new_password()` |
| `one-time-code` | `autocomplete=one-time-code` | `CredentialField::one_time_code()` |

**The mark is a hint, not authority.** It decides which list Arca offers. It never decides
the recipient. The recipient comes from what the system verified:

- the origin the courier actually reached for that document or frame, for the web;
- the app's signed identity from its channel label (arca.md §4.1), for a native app.

A false mark therefore gains nothing. An app can mark any field as a password, and the only
items offered are its own.

**What a mark produces.** For a password, new-password or one-time-code field, the app (or
for the web, the Navigator's broker, after validating the mark from the semantic tree)
asks Limen for a slot with role `credential` (a new role, §8). Arca renders the field in
the slot. The app lays it out like any other widget, and gets back a `CredentialHandle`.

A **username** field stays the app's own text field. A username isn't a bearer secret, and
the app needs it to show "signed in as". It is filled only on the same press as the
password, and only with the item the user picked. An item can mark its username private
(an email address the user doesn't want an app to learn). Then the username is a slot too.

## 3. Filling: one press, on Arca's chip

1. The credential field gets focus, or the user clicks it. Arca shows its **fill chip**
   inside the slot: the matching accounts and the recipient (§5).
2. The user picks one with a click, or with the arrow keys and Enter. frescod mints a press
   token for that press. Because the slot is Arca's surface, the token is bound to Arca's
   uid and window, and the app never sees the event (limen.md §6).
3. Arca spends the token with frescod, using a new purpose, `FILL`. The consumer allowlist
   gets `_arca-ui`, Arca's jailed credential provider (arcad itself takes no frescod
   events). On top of the checks in tabula.md §2.2, frescod requires the slot to have
   been **visible** in the sense of §4.
4. Arca marks its field as filled, showing a fixed number of bullets. It fills the app's
   username field on the same press, through Limen's `filled { username }` message (§8).
5. On submit, the app passes the handle to `arca_template_send`, or to the broker's form
   POST for the web. The courier fills the slot only after TLS to the item's recipient
   (arca.md §5, §8.3).

**Nothing else fills.** There is no fill on page load, on focus alone, from a keyboard
shortcut the app handles, or from an API call. One press fills one form, once. A form
re-rendered after the fill is a new form and needs a new press.

**Keyboard-only users** get the same path, because a KEY press inside Arca's focused surface
is a real press with a token. Tab moves focus into the slot like any widget.

## 4. Visibility: what frescod checks before a fill counts

frescod already owns the scene and hit testing, so it is the only party that can say what
the user saw. For a `credential` slot, it refuses the `FILL` consume (new refusal
`OBSCURED`) unless all of these held from 500 ms before the press until the press:

- the slot's rect was entirely inside its window and the window inside the screen;
- nothing was drawn above it: no parent content, no other window, no notification. Every
  Limen slot is a child window pinned above its parent (limen.md §5.1), so the parent
  can't draw over it; for a `credential` slot, frescod also checks other windows;
- no opacity, blur, scale under 1 or transform applied to it by the parent;
- it was at least 120 x 24 logical px;
- it did not move or resize.

The 500 ms window is the clickjacking fix that Android and the major browsers converged
on: a target that appears, or slides under the pointer, just before a click is not the
target the user meant.

### 4.1 As built in frescod (2026-11-06), step 1 of §9

Limen has no code yet, so a stand-in binds the slot. `OP_WM_CREDENTIAL_SLOT`
(0x0529, `CredentialSlotPayload { child, parent, rect: Option<[x, y, w, h]> }`) is a
window-management op; fresco-client calls it `wm_credential_slot`. It turns window `child`
(Arca's) into a slot at `rect` in `parent`. Sending it again moves the slot, and `None`
releases it. Every frame, before anything is drawn, frescod puts the slot at the parent's
position plus `rect`, gives it `rect`'s size (Arca hears the resize), and places it directly
above its parent in z-order.

**Why this covers the rules.** The parent draws only into its own window, which sits below
the slot. So it has no way to draw over, fade, blur, scale or transform the slot.

**The visibility record** is `frescod/src/credential.rs`. On each composed frame, frescod checks
every slot. The slot passes when all of these hold:

- its rect is wholly inside the parent;
- it is on screen and below frescod's strip;
- no window composed above it overlaps it, including notifications and any translucent window;
- it is at least 120x24;
- it has not moved or changed size since the last frame.

A frame where all of these hold continues the slot's visible run, and any frame where one fails
ends the run.

**When a fill counts.** In `fresco_protocol::activation`, purpose `FILL` is 7 and refusal
`OBSCURED` is 7. A `FILL` press counts only if one visible run covered the whole 500 ms before
the press and lasted until the press. A press that doesn't qualify is refused `OBSCURED` and is
not spent. Visibility is checked before focus, so a click that lands while a cover is up answers
`OBSCURED` even if the cover took focus. A `FILL` accepts a click, or Enter (or keypad Enter) in the slot; ordinary typing is
refused `WRONG_KIND`. The default consumers gain `_arca-ui` (uid 65021), the provider that
owns the slot; arcad never spends tokens.

**The frame.** frescod draws a gold ring and a key tab around each slot that is fit to fill on
this frame: in split mode as part of its chrome, in direct mode (no strip) as a layer of its own
on top. No app, and not Arca either, can draw that frame.

**Tests:**

- unit tests of the record (just appeared, moved, covered, shrunk, partly off screen) and of
  `FILL` in the token table;
- `atrium-activation-probe` in the VM, in direct and split mode. It binds a slot and checks
  that each attempt is refused `OBSCURED`: a click right after it appeared, a window over part
  of it, a translucent veil, a move just before the click, shrinking below 120x24, and sticking
  out of its window. It also checks that a slot that is still and clear fills, both before and
  after the covers go.

The probe also saves the last capture with a slot fit to fill; the ring and tab show in both
modes. Limen's role and Pergola's side are in §8.1.

### 8.1 As built: the role messages and Pergola (2026-11-06, §9 steps 2 and 3)

- **`limen/`** (new crate) holds `limen::credential`: `Purpose`, `ToChild::Configure`, and
  `ToParent::{Filled, Handle, Cleared}`. `ToParent::decode` refuses a username over 256 bytes
  or one with control characters. The broker itself (launching the child, carrying messages)
  is not built.
- **`pergola::slot`**: `Ctx::slot(id, key)` marks a node as an embed slot. After each layout the
  `App` works out its window rect (through scroll views) and queues a change when it moved;
  `Window::paint` hands changes to a `SlotBinder` after the present, in surface pixels. A slot
  that a scroll view clips even partly is released until it is whole again, so a field being
  scrolled can't be filled (each move restarts frescod's 500 ms anyway).
- **`CredentialField`** (`password`, `new_password`, `one_time_code`, optional
  `username_field`) reserves at least 120x24, is not focusable and has no key handler, and keeps
  only `CredentialState { filled, handle }`. `configure()` gives the binder the `Configure`
  message; `apply(ToParent)` takes the child's.
- **Not built**: a binder. The stand-in would send `wm_credential_slot`, which needs the WM
  socket, so it belongs to Limen's broker, not to the app.

## 5. What the user sees

- **System chrome.** The slot carries a frame and a small key glyph drawn by frescod, not
  by Arca or the app. It's the same treatment as vestibulum.md §4.3, so no app can draw a
  convincing fake that also fills.
- **The recipient**, drawn by Arca: the full origin, with the registrable domain in bold.
  For a frame whose origin differs from the top level, both origins are shown, and nothing
  is offered unless the item was saved for that frame's origin.
- **Linked origins.** `login.example.com` and `example.com` are different recipients
  (arca.md §3). The user can link them in Arca's trusted UI. That makes one item usable for
  both, with both listed on the chip. A site can't link them, and nor can an app.
- **Refusals are visible.** On `http:`, a confusable host or an `OBSCURED` press, the chip
  says why it didn't fill. It never fails silently, so a failure doesn't push the user to
  type the password by hand.

## 6. Typing, generating and saving

- **Typing.** Keys go to the focused surface, which is Arca's (limen.md §6). The app and
  the page never see them. The Navigator's rule in arca.md §8.3 becomes this general one.
- **Generating.** A `new-password` slot offers "Use a strong password". Arca creates it, and
  holds it as a pending item for the recipient. It's shown only on reveal, with a
  Vestibulum check.
- **Saving.** The courier reports the outcome of the submit to Arca (status, redirect,
  `Set-Cookie`). If it looks like a successful sign-in, Arca offers to save the typed or
  generated password. The offer appears in Arca's own surface (a notification-layer window,
  tabula.md §5.2), not the app's. The recipient saved is the one the courier reached, never
  one the app names.
- **Updating.** A `new-password` slot on a page that also has a `current-password` slot
  for the same recipient is a change-password form. On success, Arca replaces the item and
  keeps the old value in history for 30 days.

## 7. Where this can't apply, and the fallbacks

| client | what it gets |
|---|---|
| a Pergola app talking HTTP(S) | the full design: slot, handle, courier |
| the Navigator, tiers 1 to 3 | the full design; the broker places slots over the marked fields |
| the Navigator, tier 4 (the page's own engine) | the slot still takes the keys. If the page's script must read the value to work, Arca asks first ("This site's script will see this password"), as arca.md §8.3 says for tier 4 |
| a native app on a protocol the courier doesn't speak (IMAP, a database) | app-held (arca.md §4.3): after the press and a one-time grant, the value goes to the app's memory. Shown as weaker |
| Insula and ported apps with their own toolkits | no slot. Copy with a Vestibulum check (sensitive Tabula entry), or app-held with a grant. Never synthetic typing |
| passkeys | offered first on the chip when the site supports them (arca.md §8.4). Nothing to fill, nothing to phish |
| one-time codes | TOTP items in Arca, filled into a `one-time-code` slot the same way. A code the app must see (no courier) goes as plain text after the press. It is short-lived |

## 8. Interfaces

- **Limen role `credential`** (limen.md §2.8): parent to child `configure { purpose,
  username_field: bool }`; child to parent `filled { username: Option<String> }`,
  `handle { CredentialHandle }`, `cleared`. There's no message that carries the value.
- **`CredentialHandle`**: 16 random bytes, meaningful only on this app's Arca channel, for
  one submit. It expires when the slot detaches or after 10 minutes.
- **frescod**: press purpose `FILL`, consumer `_arca-ui`, refusal `OBSCURED`, the `credential`
  slot's z-policy, and the 500 ms visibility record (Fresco thread).
- **Pergola**: `CredentialField`, a widget that asks for the slot and lays it out. It
  exposes the handle and `is_filled()`, and nothing else. `TextField::secret` stays for
  secrets that aren't credentials (a passphrase the app itself checks), and its doc points
  here (Pergola thread).
- **Arca**: the slot surface, the chip, generate, save and update, and the courier's
  outcome report (arca.md A3).

## 9. Order of work

Arca is a spec today; nothing of A0 is built. Autofill is arca.md phase **A3**, and it
needs A0 (the store and channels) and A1 (the courier) first. The pieces outside Arca can
go first, because they are useful on their own:

1. frescod: the `credential` slot rules and the visibility record (§4), with a probe that
   tries to overlay, fade, move and shrink a slot and is refused each time.
2. Limen: the `credential` role, with a stand-in child.
3. Pergola: `CredentialField` against the stand-in.
4. Arca A0 and A1, then A3: the real child, fill, generate, save, and the Navigator's
   broker placing slots over marked web fields.

## 10. Decisions, with recommendations

**Accepted by the user, 2026-11-06: all five recommendations below.**

1. **A credential field is an Arca-owned embed, not an app widget.** *Recommend yes.* It's
   the only way the app never holds the value, and it makes keystroke isolation, fill and
   save one mechanism.
2. **Every fill needs a press on Arca's chip; there's no fill on load.** *Recommend yes.*
   This is a little slower than one-tap autofill on page load, which is the most common
   leak there is.
3. **Usernames are filled into the app's own field**, unless the item marks them private.
   *Recommend yes.* Apps need them to show who is signed in.
4. **No registrable-domain matching; the user links origins in Arca's UI.** *Recommend
   yes.* Some sites will need one link the first time.
5. **Third-party password managers** plug in later as Arca providers on the same slot,
   never as apps that fill other apps' fields. *Recommend: later, not now.*
