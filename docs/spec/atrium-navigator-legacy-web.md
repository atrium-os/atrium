# The legacy web lane — compatibility without a JavaScript sandbox

**Status:** design, not built. Third of the navigator specs:
[atrium-navigator.md](atrium-navigator.md) (thesis),
[atrium-navigator-backend.md](atrium-navigator-backend.md) (backend/frontend seam),
[atrium-document-profile-v1.md](atrium-document-profile-v1.md) (the document lane).
This document covers the part those three deliberately exclude: the existing web, which
is not going to be rewritten for us.

---

## 1. The premise correction

"Run it server-side" is sometimes heard as "the site must change". It does not. Running
an engine against an unmodified site is a **rendering proxy**: the site sees an ordinary
browser fetching it over HTTP. No cooperation, no modification, no negotiation. This is
proven at scale — Opera Mini served a very large user base for years with a thin client
and a server-side engine, and enterprise browser-isolation products ship the same shape
today for exactly our security reason.

And the second correction, which matters more: **"server-side" may mean localhost.**
Location transparency is already the architecture's position (atrium-navigator.md §3.1) —
the engine runs in a *jail*, and where that jail runs is a deployment knob. On the local
machine you get compatibility with no third party seeing your traffic and no added
latency; on a remote host you get the thin-client and battery cases. One mechanism, two
placements.

## 2. The thesis, applied to its hardest case

> **We never build a JavaScript sandbox. We build a jail around someone else's engine.**

The whole project rests on the claim that V8's in-process sandbox exists only because the
OS never sandboxed processes properly. The legacy lane is where that claim gets tested
rather than asserted. We take an existing engine (Servo, MPL-2.0, already position 5 of
the D6 build order) and treat it as **untrusted software**: Portcullis jail, zero
capabilities, no filesystem, network only through the brokered fetcher, and one output —
a scene graph on a pipe.

An engine zero-day then lands the attacker in a jail with nothing in it. Contrast a
browser, where a renderer compromise lands in a process holding a large, privileged IPC
surface to its parent. The work here is integration, not invention: we are not writing a
JIT, a sandbox, or a DOM.

## 3. "Sites need JS" is four populations, not one

| | population | answer | lane |
|---|---|---|---|
| **1** | documents using JS incidentally (analytics, banners, menus, lazy-load) | content is already in the HTML — the normalizer needs no JS at all | document |
| **2** | SPA-rendered documents (empty shell, JS builds the DOM) | JS at **conversion** time, not view time (§4) | document, after conversion |
| **3** | genuine applications (mail, maps, design tools) | the jailed engine (§2). No way around it and no shame in it | legacy |
| **4** | light necessary interactivity (validation, tabs, sort, autocomplete) | a **total** declarative vocabulary (§5) | document |

Populations 1, 2 and 4 are the majority of what people call "browsing", and none of them
requires script execution *while the user is reading*. That is the leverage.

## 4. JS at conversion time

The normalizer (document profile §6) is already jailed, offline, zero-capability, and its
output is content-addressed. Give it a JS engine and it resolves population 2: run the
page once headlessly, snapshot the resulting DOM, emit a profile-conformant static
document. The reader then gets the fast, zero-authority lane with all six guarantees
intact, and the conversion is amortised across every reader of those bytes.

**Hard rule: never share a conversion made with credentials.** A prerender of a logged-in
page is a render of someone's session. Personalised conversions go in a private or salted
dedup domain (tessera-fs.md §20); only anonymous fetches may produce artifacts in a shared
domain. Getting this backwards turns the dedup win into a data leak.

Honest cost: prerendered artifacts go stale, and staleness is a Nomenclator freshness
question, not a store question.

## 5. A total interaction vocabulary

Population 4 needs *interactivity*, not *Turing-completeness*: show/hide, tabs,
accordions, validation, sort and filter, autocomplete. A bounded declarative vocabulary —
named states and transitions, data bindings, constraint-based validation, an expression
language with no loops and no recursion — covers these while preserving every guarantee in
the document profile: it is deterministic, analysable, and provably terminating.

**The discipline to write down now:** every declarative interaction language in history
has grown toward Turing-completeness under feature pressure. Additions must preserve
totality *by construction*, and "just add a loop" is the change that ends the document
lane's guarantees. If a feature cannot be expressed totally, it belongs to an app, not a
document.

### 5.1 Where JS runs matters more than whether it runs

A concern worth settling before §5.2: does admitting JS anywhere forfeit the benefits that
come from being JS-free? No — because the benefits come from JS not running **in the
reader's session**, and conversion-time execution (§4) does not.

| | JS at view time (a browser) | JS at conversion time (the normalizer) |
|---|---|---|
| fingerprinting | profiles the reader's machine | profiles the *converter's* hermetic environment — a fixed synthetic host. The reader is not present |
| timing side channels | live against the reader | the script is not running while anyone reads |
| credentials | the user's session is in scope | no session; shared conversions are anonymous by rule (§4) |
| output | arbitrary ongoing DOM mutation | a static artifact, validated against the profile before anyone sees it |

So the JS-free-at-view invariant is preserved exactly. The question is never "JS or no JS",
it is "which side of the conversion boundary".

### 5.2 Transcription: what it cannot be

The appealing idea is a jailed **transcriber** that converts a site's JavaScript into
native declarative interactions once per unique script, amortised by content-addressed
dedup. The economics are attractive and the dedup insight is sound. As a general
JS-to-declarative *compiler*, however, it cannot work, and the reasons are structural
rather than engineering effort:

1. **The target is deliberately less expressive.** §5's vocabulary is total — no loops, no
   recursion, provably terminating. JavaScript is Turing-complete. Translating a
   Turing-complete language into a total one is not possible in general; it is a
   computability fact, not a gap to close. A transcriber can only ever succeed on whatever
   subset happens to fall inside the total fragment.
2. **JavaScript's behaviour is not a function of its syntax.** `eval`, `Function()`,
   `Proxy`, getters and setters, prototype mutation, dynamic property access, monkey-patched
   builtins — real-world semantics depend on runtime values. Every static JS analyser is
   approximate for this reason, and minified bundles are the adversarial case.
3. **The artifact is a framework, not logic.** Real sites ship a bundled framework plus app
   code. There is no discrete "this function toggles the menu" to lift out; the behaviour
   emerges from a framework runtime, a component tree, and state.
4. **Equivalence is unverifiable.** A wrong transcription yields a subtly misbehaving page,
   which is worse than an honestly unsupported one. Proving two programs equivalent is not
   generally possible, and per-site validation would destroy the "once per script"
   economics that motivated the idea.
5. **The dedup win lands on the wrong half.** A given minified jQuery is one hash across
   millions of sites — genuinely valuable. But app bundles are per-site and often
   per-deploy (content-hashed filenames, rebuilt continuously), and the app bundle is
   exactly the part that implements the site's behaviour.

### 5.3 What does work: recognise and record, do not translate

Reframed, the idea is valuable — as a **recogniser and a recorder** rather than a compiler.
Three tiers, increasing in cost and decreasing in fidelity guarantee:

**Tier 1 — content extraction.** No JS at view time, content taken from the HTML. Handles
population 1. Already the plan.

**Tier 2 — prerender plus recorded state machine.** The normalizer already runs the page
once (§4). Extend it from capturing the *DOM* to capturing *observable states*: drive the
page — open each menu, each tab, each accordion — and record the resulting DOM diffs as
named states and transitions in the §5 vocabulary. This is black-box: it never needs to
understand the code, only the states it exhibits, which is precisely what our total
vocabulary can express. It covers most of population 4.

*Limits, stated so they are not discovered later:* only finite, enumerable, deterministic
state spaces; nothing data-dependent (search results, infinite scroll, anything
server-fed); exploration is a crawling problem with combinatorial blow-up; and a page may
behave differently under real input than under exploration.

**Tier 3 — known-artifact substitution by hash.** Here the dedup insight pays off
properly. A content hash identifies an exact artifact — this *is* jQuery 3.6.0, this *is* a
known analytics SDK, this *is* a known consent banner — and the response is not to
translate it but to **substitute a known native behaviour or a no-op**. High confidence,
because the hash pins the exact bytes rather than inferring intent.

*Cost:* a maintained recognition list, with the same arms-race and staleness properties as
filter lists; every version bump needs an entry.

**Tier 4 — the jailed engine** (§2), for population 3, if we ship one at all (§5.4).

### 5.4 The engine decision should be empirical, and deferred

Whether to ship a legacy engine at all is a product decision, not a technical one:
without it, genuine web applications simply do not work, and that may be an acceptable
scope for a reading-first system.

**Recommendation: build tiers 1–3 first and defer the engine decision until it can be
measured.** The experiment is concrete — take a corpus of real sites and measure what
fraction is usable under tier 1, then tier 1+2, then tier 1+2+3. That number decides
whether the largest single piece of work in D6 is worth starting, and it is cheap to
obtain relative to the engine itself.

### 5.4.1 The experiment, run — and what it says

§5.4 asked for a concrete number and deferred a decision on it. The number now exists,
from `navigator-prerender` over a 104-document corpus of real sites (85 with script).
It is reported by the converter itself, so it can be re-run rather than believed.

> **Reproducing these numbers.** The corpus is not in this tree — it is fetched pages, and
> checking it in would pin content that changes. Every figure below is printed by the
> converter's own corpus driver, so the claim and its evidence are the same program:
>
> ```
> cd navigator-prerender
> cargo build --release
> PRERENDER_EXPLORE=1 PRERENDER_NET=1 ./target/release/prerender <corpus-dir>
> ```
>
> The driver reads `manifest.tsv` in that directory (`filename<TAB>url`) to resolve each
> document's own URL. Numbers quoted here are from a 104-document corpus; a different
> corpus will give different numbers, and the shape of the finding is what should be
> compared, not the digits. **Nothing in this tree verifies these figures**, so treat them
> as a dated measurement rather than an invariant — they are stated with the corpus size
> beside them for exactly that reason.

**Tier 2's prerender half completes on 82–84% of scripted documents.** That is the
fraction whose scripts all run without a gap in our host environment — the number §5.4
implicitly asked for, and on its own it reads as an endorsement of tier 2.

**It is the wrong number to decide on.** "Every script ran" is not "the artifact is
better". Measuring the visible text a conversion actually adds:

| outcome of running the scripts | documents |
|---|---|
| more content than the raw HTML | 19 |
| no change | 64 |
| **less** content — tier 1 is better | 2 |

So **tier 1 alone is adequate or better for 66 of 85 scripted documents**, and the median
document gains nothing from tier 2 at all. The two regressions are hydrating apps that
tear down server-rendered content and rebuild it emptier; for those, converting is worse
than not converting, which §5.4 did not anticipate and which the tier model must handle
(see §5.4.2).

**What this says about the engine decision.** The case for tier 4 is population 3 —
genuine applications — and nothing here weakens or strengthens that. What it does settle
is the case for tier 4 *as a way to rescue documents*: it would not. The one document
that loses its content runs all 70 of its scripts **cleanly**; engine fidelity is not its
constraint. What it lacks is cross-origin data our privacy boundary refuses. A better
engine fails identically. **The remaining gap in the reading lane is a data-access
boundary, not an execution one**, and that is a policy question rather than an
engineering one.

*Caveats, because the number will be quoted:* the corpus is 104 documents chosen for
breadth, not sampled from traffic; "usable" here is measured as visible text, so a
conversion that improves layout or ordering without adding words counts as no change; and
the 19 gains are not uniform — one document accounts for 92,432 of the 95,858 characters
added. The verdict split is a heuristic, not a browser diff (§4).

### 5.4.1a A second corpus, chosen to break the finding

The first corpus is dominated by technical documentation — Wikipedia, W3C, RFCs, language
docs — which is the most static content on the web, so "tier 1 is enough" could easily be
an artifact of *what was measured*. A second corpus of 67 pages was fetched deliberately
from the populations most likely to contradict it: commerce, civic services, forums,
recipes, listings, non-English news, and app-shaped sites.

It splits into three populations, and keeping them apart is what makes it readable:

| population | n | grew | unchanged | worse | net text |
|---|---|---|---|---|---|
| **bot-wall** — a challenge page, not the site | 22 | 1 | 19 | 2 | −6,447 |
| **app shell** — real page, no content in the HTML | 7 | 0 | 7 | 0 | 0 |
| **document** — genuine content | 38 | 5 | 31 | 2 | −6,863 |

**The finding survives contact with the population chosen to break it.** Of 38 genuine
documents, 33 are unchanged or worse for running their scripts, and the net text change is
NEGATIVE. That is a stronger result than the first corpus (66 of 85), on exactly the
content where tier 2 was expected to earn its keep.

**The app shells are the honest exception, and they are population 2 of §3.** Seven
documents have no content in the HTML at all: tier 1 has nothing to publish, and tier 2
produces nothing either, because what they need is cross-origin, authenticated data this
converter refuses. Neither tier works. This is the same boundary as open question 6, met
from the other direction — and it is the clearest statement of what a reading-first system
does *not* cover.

**A methodological finding that is also a product one.** A third of these fetches returned
a bot challenge to a plain GET. That is a property of fetching without a browser — and the
Navigator's converter fetches exactly that way. **A meaningful fraction of commercial sites
will serve a challenge page rather than content to any converter that does not execute
JavaScript at fetch time and present a browser's fingerprint.** It is not a conversion
problem, it is an access problem, and it arrives before any of this spec's machinery runs.

*The obvious confound was tested and ruled out.* The fetcher advertised a research
user-agent, which could plausibly have CAUSED the challenges. Re-fetching five of the
challenged URLs with a current Chrome user-agent returned a challenge every time, with zero
words of content. So the trigger is not the UA string — it is TLS fingerprint, header
shape, IP reputation, or headless detection, none of which a polite string fixes. The first
corpus, fetched identically, was not challenged at all: the split is by site category, not
by fetcher.

**What this costs the architecture — stated more carefully than at first.** An earlier
draft of this section claimed that fetching as the reader would destroy the dedup property
outright. That was too strong, and the correction matters for §5.4.1b. Artifacts are keyed
by the hash of the INPUT BYTES, so if two readers obtain the same bytes they share the same
artifact: what reader-side fetching loses is *fetch*-once-serve-many, not
*convert*-once-serve-many — and conversion, which runs the page's JavaScript, is the
expensive half. Dedup survives reader-side fetching for any page that is not personalised.

What does not survive is unattended conversion. A challenged site cannot be converted ahead
of a reader asking for it, by anyone, because nothing in the pipeline can obtain the bytes.

### 5.4.1b Settled: challenged sites, and what the Navigator will not do

The comfortable answer would be that challenged sites are transactional and a reading-first
system need not claim them. The evidence refuses it. Of the 22 challenged in §5.4.1a, about
half are exactly the reading a document browser exists for: Stack Overflow answers, AP and
NPR and Economist articles, wikiHow, BBC Good Food recipes, Khan Academy, Consumer Reports.
Declining them is declining a large part of the point.

**The decision has three parts.**

**1. The Navigator identifies itself honestly and does not impersonate a browser.** Matching
Chrome's TLS fingerprint and header order would get past most of these walls today. It is
rejected on two grounds, and the second is the one that settles it: it is an arms race with
a well-funded other side, so it fails eventually anyway; and it is a lie told by software
on the reader's behalf to a server that asked a direct question. A system whose entire
argument is *the reader's agent works for the reader* cannot start by misrepresenting who
is calling.

**2. A refusal is surfaced, never silently converted.** A challenge page is not the site,
and converting one produces an artifact that looks like a successful conversion of a page
that says "Just a moment...". The converter must detect the shape and mark the artifact
REFUSED-BY-ORIGIN, so the reader is told the site declined rather than shown a blank
document. This is the same discipline as §5.4.2's floor: never publish something that looks
like a result and is not.

**3. Reader-assisted import is the supported path, and it is not a workaround.** Where the
reader can already see a page in a browser they have, that page's bytes can be handed to
the converter. No impersonation is involved — the reader fetched it, as themselves, and is
entitled to it. Conversion then proceeds normally and the artifact dedups by content hash
like any other. This is strictly weaker than automatic conversion and should be described
that way: it does not scale to crawling, and it requires an act by the reader.

**What this means for scope.** The Navigator serves the open web automatically, and the
challenged fraction on the reader's own request. It does not pretend to be Chrome, and it
does not pretend a challenge page is a document. The cost is honest and visible: some sites
will not work without the reader's involvement, and a few will not work at all.

*Caveat on the caveat:* these 67 were chosen adversarially rather than sampled, so their
proportions are not the web's. What transfers is the direction — tier 2 does not rescue
this population either — not the percentages.

### 5.4.1c Settled: the page network stays SAME-ORIGIN

Open question 7 asked where the data boundary sits for hydrating pages, since §5.4.1 shows
their loss is data access rather than engine fidelity. The candidate relaxation was
concrete: admit same-**site** requests, not merely same-**origin**. It is the obvious move,
because a hydrating page's data commonly lives one label over — `bbc.co.uk` fetching
`idcta.api.bbc.co.uk` is same site, different origin, and the stricter rule refuses exactly
the request the page needs.

It was implemented behind a flag and measured on both corpora rather than argued.

| | same-origin | same-site |
|---|---|---|
| corpus 1 — page requests admitted | 88 | **1,082** |
| corpus 1 — documents gaining content | 19 | 19 |
| corpus 1 — documents demoted to tier 1 | 1 | 1 |
| corpus 2 — page requests admitted | 54 | 62 |
| corpus 2 — documents gaining content | 6 | 7 |
| corpus 2 — documents demoted to tier 1 | 2 | 2 |

**It buys one marginal document and admits twelve times the traffic.** Not one document is
rescued from demotion. bbc.co.uk — the case that motivated the question — is not recovered
at all: its text still falls from 14,654 characters to 5,063.

**And the traffic it admits is the wrong kind.** 989 of the 1,082 requests come from a
single document, to a single first-party host, in a burst that produces no content
whatsoever. That is the shape of telemetry, not of data a page renders. The reason is
structural rather than incidental: **first-party analytics lives on same-site subdomains
precisely because third-party blocking does not reach there.** Same-site is the boundary
modern tracking is built to sit inside, so relaxing to it concedes the most while gaining
the least.

There is a second reason, weaker but pointing the same way. Same-site cannot be computed
from a URL — it needs the Public Suffix List, a downloaded file that drifts, and a boundary
that depends on a fetched list is weaker than one computable from the URL alone.

**Settled: the page network remains same-origin GET.** The hydrating population is not
recovered by a boundary we would be willing to move, and the boundary that would recover it
is the one we are least willing to move. For that population the answer is tier 1 (§5.4.2)
and, where the reader wants more, reader-assisted import (§5.4.1b) — the same answer open
question 6 reached from the other side, which is some evidence it is the right shape.

*The flag stays in the code, off by default, so the measurement can be repeated rather than
believed.*

### 5.4.1d Settled: the converter's OWN subresources — typed cross-origin GET

§5.4.1c governs requests a page's *scripts* make. It says nothing about the converter's
own fetches of the stylesheets and fonts a document declares, which the document lane
needs in order to be laid out at all (Profile v1 admits web fonts as required inputs).
Measured on the 171-document corpora, with the stylesheets and fonts already fetched for
the profile's ceilings:

| | cross-origin | documents where ALL of them are cross-origin |
|---|---|---|
| linked stylesheets | 204 of 634 | **25 of 133** documents with linked CSS |
| `@font-face` sources | 602 of 1,140 | **35 of 80** font-using documents |

Same-origin would give GitHub and every Discourse forum no CSS at all. The cross-origin
hosts are overwhelmingly the site's **own** CDN (`githubassets`, `cdninstagram`, `guim`,
`bbci`, `scdn`, `elpais`), which same-*site* would not rescue either. The main genuine third
party is Google Fonts.

**Settled (user, 2026-09-22): the converter may GET stylesheets and fonts from any
origin, under structural rules:**

1. **Only what the document declares:** `<link rel=stylesheet>` (and inline `<style>`),
   `@import` (depth ≤ 4), `@font-face src`. Never a URL a script computes, and never a
   `url()` of any other kind through this path.
2. **GET, no credentials, no Referer, no cookies accepted.** The CDN learns that the
   document was converted, and by which address. It learns that **once per conversion,
   which is shared by every reader of it**, not once per reader as a browser would report.
3. **Typed:** a response is kept only if it canonicalizes as a font (`navigator-fonts`) or
   is accepted as CSS. Anything else is discarded and counted. A beacon endpoint yields
   nothing usable.
4. **Counted and reported by host**, like every page-network refusal, so the traffic is
   visible rather than assumed.
5. **Through the `Fetcher` seam**: in the shipped design, the brokered fetcher (backend
   §2) is the only holder of network.

**No blocklist**, for the reason §5.4.1c's code gives: a filter list is an arms race and
is wrong the day it ships. Declared-only plus typed responses is the structural floor, and
it is the same shape as the page-network rule rather than an exception to it.
(Recorded explicitly because the option first put to the user mentioned a tracker
blocklist; the tree has none and deliberately so. Adding one on top is a separate
decision.)

### 5.4.1e Proposed: anonymous cookies, before Arca (2026-11-05)

*Asked by the user, 2026-11-05: "I'm not sure we designed cookies and cookie store for
Navigator. We may need it before Arca." Not built.*

**Today there are no cookies at all.** `navigator-fetch` sends no `Cookie` header and drops
every `Set-Cookie` (`http.rs`). The converter's `document.cookie` reads `""` and discards
writes (`boa_impl.rs`), and `navigator.cookieEnabled` is `false`. That was right for
anonymous, shareable conversions, but some pages need a cookie to show anything:

1. **Cookie checks.** A redirect sets a cookie and points back to the page, expecting the
   cookie on return. Without a jar, that's a redirect loop or a "please enable cookies" page.
2. **Consent and interstitial walls.** The reader's "accept" or "continue" is stored as a
   cookie, and the content appears only on the next request that carries it.
3. **State within one visit**, such as a region choice, a basket or a paywall meter.
4. **Challenge clearance** (`cf_clearance` and similar). This is **out of scope:** §5.4.1b
   says challenges are never solved automatically, and a jar must not become a way round
   that.

[Arca](arca.md) is the long-term home for cookies (Arca §8.1), and logins wait for its
phase A2. So the question here is narrower: **is there an anonymous, non-persistent jar we
can ship now, and does it rescue enough pages to be worth it?** §5.4.1c's method applies:
measure before deciding.

**The design, if the measurement says yes.**

- **Cookies are off by default and turned on per view.** A view's first conversion stays
  anonymous and shareable. A view switches to *cookied* when either:
  - the site needs it: a redirect sets a cookie and returns to the same origin, or the
    response says cookies are required; or
  - the reader acts: they press a consent or continue control, which the document lane
    replays as a transition.

  From then on, that view's conversions are **private to the user and never shared**, the
  same rule as credentialed conversions (threat model §6.7).
- **The jar lives only in memory, for one view.** It is never written to disk and never
  kept past the view's close. Nothing persists across visits until Arca (phase A2) exists,
  so a consent choice is asked again on the next visit. That cost is deliberate: a
  persistent per-site store is a tracking store, and it needs Arca's encryption and
  clearing rules.
- **The jar lives in Arca, as its first slice** (*decided by the user, 2026-11-05*). This is
  a minimal `arcad` with only the web compartment: cookie records, Set-Cookie validation
  and RFC 6265bis matching (Arca §8.1). It has no app keys, passwords or passkeys yet. It
  starts memory-only, so it holds nothing across a restart; persistence follows once
  Vestibulum's unlock is wired (Arca §6). No jar ever lives inside a Navigator process, so
  the rule "secrets live in Arca" holds from the first cookie.
- **A per-origin fetcher sends cookies.** Today one `navigator-fetchd` serves every origin
  of a conversion. Cookied requests go through a separate fetcher for one (partition,
  origin), which is Arca's courier (Arca §5): it receives the `Cookie` value from Arca only
  after TLS, hands back `Set-Cookie` as structured records, and never follows a redirect
  to another origin. Anonymous fetches keep today's path.
- **The broker issues tickets** (Arca §8.2) naming the reason, the SameSite context and the
  gesture. Converters never hold one.
- **Which fetches carry cookies:** the main document and the page's same-origin GETs in a
  cookied view. Never §5.4.1d's cross-origin stylesheets and fonts, and no hints, prefetches
  or image-header peeks until a ticket covers them.
- **Credentialed conversions are keyed by URL plus a jar version.** A change in the jar,
  such as a consent being accepted, makes a fresh conversion. Progressive delivery (first
  display, images, the scripted version) works unchanged. Clearing a site deletes its
  conversions.
- **Script cookies.** The converter's `document.cookie` gets that origin's non-`HttpOnly`
  cookies, which is the web's own contract (Arca §12, decision 4). Writes made during
  exploration go to a scratch copy and are dropped. Only the transition the reader actually
  takes writes to Arca, so exploration clicks never change the reader's real session.
- **Expiry uses Arca's clock, never the converter's** (threat model §2.6). Limits and
  eviction are Arca's.
- **The reader sees it.** The trusted UI marks a cookied view and offers "forget this site".
- **Cookies go only to the page's origin.** The page network is same-origin GET (§5.4.1c),
  and §5.4.1d's cross-origin stylesheets and fonts stay cookieless. In effect, the jar
  holds one origin per view.
- **The rules are Arca's.** Parsing, the public suffix list, `__Host-` and `__Secure-`,
  `Secure`, `SameSite=Lax` by default and the size limits are one crate inside Arca.
- **No logins.** Form POSTs stay refused. A GET that logs the reader in (a magic link)
  produces a cookie that dies with the view and is never stored. The trusted UI shows that
  a view is cookied.

**What a compromise reaches.** A compromised converter reads that view's non-`HttpOnly`
cookies, which the site's own scripts read anyway. A compromised per-origin fetcher reads
one origin's cookies for one request, and they hold no login. That is far below Arca's bar, which is why logins still
wait for Arca.

**The measurement to run first (both corpora, offline replay where possible):**

| question | how it is counted |
|---|---|
| how many documents hit a cookie check | a redirect chain that sets a cookie and returns to the same origin; "enable cookies" text that isn't a §5.4.1b challenge |
| how many gain content with a jar | text and node counts with and without the jar, for those documents only |
| how many consent walls clear on replaying "accept" | documents whose content appears only after a consent transition plus its cookie |
| what sharing it costs | the share of views that turn cookied, and so become private |
| what it must not change | documents with no cookie check stay byte-identical with the jar on; challenge pages stay REFUSED-BY-ORIGIN |

If the jar rescues only a handful of documents, the Navigator side waits, as same-site did
in §5.4.1c. Arca's cookie slice is built either way, because logins need it.

**Build order:**
1. the measurement above;
2. the per-origin fetcher;
3. the minimal `arcad` jar;
4. tickets from the broker;
5. credentialed conversions;
6. `document.cookie`;
7. the reader-facing UI.

**Measured (2026-11-06): step 1.**

*Method.* Every candidate URL of both corpora was used: 146, not just the 122 kept. The
23 that failed at selection (403s, 000s, a 400, a 202) are exactly where a cookie check
would hide. Each URL got three live fetches of its main document, back to back, with the
corpus UA and flags:
- **A**, with no jar, as today;
- **B**, with a throwaway in-memory jar for that one redirect chain (curl's cookie
  engine, an experiment harness, never shipped);
- **A2**, with no jar again, as the control.

A difference counts only where B differs from A and A2 agrees with A. Query strings are
not compared, because sign-in redirects carry per-request parameters. Nothing ran a
script, so no challenge cookie could be computed, and nothing was replayed that the server
did not itself send. Fetched from India, 2026-11-06, 01:12–01:35 IST.

| question | result |
|---|---|
| servers that set any cookie in the main document's chain | 57 of 146 |
| **a redirect sets a cookie and returns to the same origin** | **7**: weather.com, mail.google.com, calendar.google.com, web.whatsapp.com, nature.com, sciencedirect.com, old.reddit.com |
| …and the outcome changes with a jar | **2**: nature.com, sciencedirect.com |
| **content gained with a jar** | **1**: sciencedirect.com, 400 → 200, visible text 532 → 5,347 characters, the homepage instead of an error page. It was one of the 23 dropped at selection. |
| "enable cookies" walls (a short page asking for cookies, not a §5.4.1b challenge) | **0**. One long page mentions enabling cookies in its content. |
| consent walls (content hidden until consent) | **0**. 43 pages run a consent manager (OneTrust, Didomi, Sourcepoint and others), all as banners over content that is already there. |
| challenges and refusals | 6, all unchanged with a jar (none became readable, none changed outcome) |
| no cookie check, jar on | 139 URLs: B equals A byte for byte on 91. Where the page is stable (A2 = A, 88), B = A on 86. The other 2 (spiegel.de, doc02) differ in bytes with identical text and element counts: page churn. With no redirect, a fresh jar sends no cookie on the first request at all, so a document without a cookie check is unchanged by construction. |

*What it says.*
- **The jar rescues one page in 146.** The other real cookie check is nature.com's
  (`?error=cookies_not_supported` without a jar). It covers that publisher's articles,
  but the test article is a 404 either way. Both are academic publishers.
- The remaining five cookie-setting redirects change nothing. Two are Google sign-ins,
  which are logins and wait for Arca.
- By this section's own rule, **the Navigator side waits**, as same-site did in §5.4.1c.
  Arca's cookie slice is still built, because logins need it.
- *Geography is a stated limit.* These are a reader's results from India. An EU reader
  is shown consent walls where this run saw banners (lemonde, spiegel, elpais and
  corriere are in the corpus). The measurement should be repeated from an EU vantage
  before the decision is final.

*Two design refinements the data asks for.*
1. **The "site needs it" trigger should be narrower.** Turning a view cookied whenever a
   redirect sets a cookie and returns to the same origin would make 7 of 146 views (4.8%)
   private, and only 2 of them need it. A narrower trigger turns a view cookied only when
   the no-jar chain **ends in an error status, a loop, or a URL naming a cookie error**.
   On this data it catches exactly sciencedirect and nature.
2. **A banner is not a reason to go private.** Dismissing a consent banner whose content
   is already shown should be a local transition: the banner hides and no jar is made.
   Otherwise the 43 pages with banners (29% of views) would turn private the moment a
   reader clicked "accept". Only a wall, which is content withheld, should turn a view
   cookied.

Data: `~/src/navigator-corpus/cookies-2026-11-06/`. `fetch.py` fetches and `analyze.py`
reproduces the table; `cookies.tsv` has one row per URL.

### 5.4.2 Never emit an artifact worse than the input

Because tier 2 is tier 1's DOM plus whatever the scripts changed, both artifacts are in
hand at the end of a conversion and the choice costs one extra serialization. The
converter therefore **measures and does not decide**: it reports the document before and
after, the visible text of each, and the transitions it recorded. A separate policy
chooses, with a threshold an operator sets.

The default keeps tier 2 unless it retained under 80% of the visible text. On this corpus
that demotes exactly one document. The threshold is far from both edges — the
catastrophic case retains 0.346 and the only other loss retains 0.996 — so it is chosen
with margin rather than tuned.

Two exemptions are as load-bearing as the rule: a document with almost no visible text has
lost nothing when a ratio over a handful of characters swings, and a document with no
scripts has no tier 2 to publish.

This is a **floor, not a quality check**. A page that replaces good content with an equal
volume of worse content passes it.

### 5.4.3 The state recorder: JS as an oracle, not a producer

§5.3's tier 2 has a second half — recording observable states — and the measurement above
changes how cheap it is.

The page's own code is the only thing that knows which of its elements do something. So it
is run, and **its output is discarded**: what survives is a description of what each
interactive element *does*, expressed against the tier 1 document. Candidates are not
guessed; they are the handlers the page actually registered. Every probe is reverted, and
the artifact is byte-identical to one produced without exploring — verified against a
control arm, not asserted.

**93% of recorded transitions are a single attribute write.** Over the corpus: 314
transitions across 30 documents, 266 anchored to the tier 1 document, and 291 of them
carrying no content at all — because the content they reveal is *already in the tier 1
document, merely hidden*. The commonest interactive element on the web is a class toggle.

This matters for §5.3's stated limits. The combinatorial blow-up it warns about is real for
*exploration*, but the **recording** is small: for 93% of transitions it is a node path, an
attribute name, and a value, which is already expressible in the §5 vocabulary. Only the
remaining 7% need content carried, and that is bounded (32 effects and 16 KiB per
transition, with overflow counted rather than dropped silently).

**Safety, which is not optional here.** Exploration *simulates a user*, which prerendering
does not, and a simulated click must not be able to act. Four invariants:

1. **No credentials, ever.** Conversions are shared through the CAS store, so they are
   anonymous by construction; there is no session to damage.
2. **The page network is CLOSED during exploration** — not restricted to same-origin GET.
   Destructive GETs exist (`/delete?id=`), and closing the channel costs nothing because
   tier 2 excludes data-dependent states by definition (§5.3).
3. **Navigation is refused** — `location.assign` is a no-op and history cannot leave the
   document.
4. **Exploration is a separate opt-in mode.** Prerendering runs the page's own code;
   exploration acts on the reader's behalf, and the two deserve different permissions.

### 5.4.4 A refuted alternative: protected-subtree execution

Recorded so it is not re-proposed. The idea was to make hydration non-destructive *by
construction*: mark parser-produced nodes un-removable, so a page may add, decorate and
reorder the server's markup but never destroy it.

It works, and it is the wrong answer. It rescues the one regression — but the rescued
content then appears **twice**, because the client re-render is added alongside the
original it meant to replace. And the cost lands on documents that were healthy: one
gained 935 duplicated words, its code samples printed twice. It fires on 12 documents, of
which one had a problem. It does not prevent the failure, it exchanges missing content for
duplicated content. The tier 1 fallback of §5.4.2 is strictly better for the case it was
built to rescue.

### 5.4.5 Proposed: recording what the state recorder cannot — data, input, time

*Status: proposal (2026-11-04), not settled. Nothing here is built; §5.4.5.6 says what
would have to be measured before any of it is.*

§5.4.3 records a page's **finite, data-free** behaviour: each handler's effect, expressed
against the tier 1 document. §5.3 names what that leaves out. Four cases, in order of how
much of the web they decide:

1. **Server-fed content** — search results, infinite scroll, "load more", live prices,
   anything behind a login.
2. **Free-form input** — typing, validation-as-you-type, search-as-you-type, form submit.
3. **Large state spaces** — many toggles whose combinations multiply.
4. **Time** — carousels on a timer, rotating banners, clocks and countdowns.

The proposal extends the same move to each: **run the page's code as an oracle at
conversion time, observe what it does, and record the behaviour as a declarative
description in §5's vocabulary — never the code.** §5.2's objection does not apply,
because nothing is translated: the converter does not read the JS, it watches it, and
what it records is checked by re-running (§5.4.5.5) rather than trusted.

#### 5.4.5.1 Server-fed content: record the binding, not the data

The page's code for this population is nearly always the same shape: *build a request,
fetch it, pour the response into a template.* Exploration sees both halves — the request
the page made, and the DOM it produced from the response — so the converter can record a
**data binding**:

- a **request template**: method (GET only, §5.4.1c), same-origin URL with named
  parameters (`/search?q={query}&page={cursor}`), and where each parameter comes from — a
  form field, a recorded constant, or the previous response's cursor;
- a **response mapping**: which response fields fill which template slots, and the
  repeated block a list response is stamped into.

**Inference without opening the network.** §5.4.3's invariant 2 (page network closed
during exploration) stays. The responses the binding is learned from are the ones the
prerender already fetched under §5.4.1c, replayed from the conversion's cache. To attribute
DOM text to response fields unambiguously, the converter replays **perturbed copies**:
each string field replaced with a unique marker, each array lengthened and shortened. A
marker that surfaces in the DOM names its slot; a block that repeats with the array names
the template. Fields that never surface are not part of the binding. This is differential
observation, not static analysis, and it needs no request the prerender did not already
make.

**At view time** the Navigator — not the page — issues the request through the brokered
fetcher (backend §2) under the same rules as any page fetch, and fills the template. Three
properties keep this inside the document lane:

- **Data is inserted as text, never as markup.** A response cannot add elements, styles or
  handlers the binding did not declare, so a hostile or compromised endpoint can return
  wrong text and nothing else.
- **The binding is shareable; the data is not.** The artifact holds the template and
  mapping only, so it dedups like any conversion. Responses are per-reader. Where the
  origin needs the reader's session (a logged-in feed), the request goes out with that
  origin's own partitioned credentials (§7.7). The page's code never sees them, because the
  page's code is not running.
- **The request is declared, not computed.** No script runs at view time, so the request
  carries what the template says and nothing else: no fingerprinting, no beacons riding
  along.

Infinite scroll and "load more" are the same binding with a cursor parameter, driven by a
declared trigger (end of list reached, or the button) instead of a scroll listener.

*Where it fails, stated now:* GraphQL and other POST-for-reads APIs (refused by the GET
rule; a separate decision, not a quiet exception); responses the page decrypts, decodes or
heavily transforms before display (the markers do not surface verbatim); signed or
nonce-bearing request URLs that cannot be templated; WebSocket and server-sent streams.
Each is counted, not silently dropped.

#### 5.4.5.2 Free-form input: classify the handler, do not enumerate the input

Typed input cannot be explored exhaustively, but the handlers bound to it fall into a few
shapes, and each has a declarative target:

| what the handler does | recorded as |
|---|---|
| shows or hides a message depending on the value | HTML constraint validation (`required`, `pattern`, `minlength`, `type=email`, …) where the observed accept/reject behaviour matches one, checked on generated probes |
| fetches suggestions as you type | a data binding (§5.4.5.1) whose parameter is the field |
| filters or sorts a list already in the page | the vocabulary's filter/sort over the existing nodes |
| submits the form | a request template, issued only on the reader's explicit submit, like a native `<form>` |

**Finite inputs are explored, not classified:** a `<select>`'s options, checkboxes and
radio groups have enumerable values, and their effects record like any §5.4.3 transition.

**Never executed during conversion:** anything that would send a mutating request. Form
submission is recorded as a template and only ever issued by the reader, at view time,
through the same fetcher, so conversion can never act on anyone's behalf (§5.4.3
invariant 1).

*Where it fails:* rich-text editors, drag and drop, canvas drawing, anything whose effect
depends on the full history of input rather than its current value. Those are application
behaviour (population 3), not document behaviour.

#### 5.4.5.3 Large state spaces: record independent transitions independently

The blow-up §5.3 warns about comes from exploring *combinations*. Most combinations need
not be explored. Two transitions whose effects touch disjoint subtrees and whose
preconditions do not read each other's writes **commute**, so each is recorded once, alone,
and the reader's sequence is replayed in any order with the same result. Only transitions
that interfere — one's effect is the other's precondition, or both write the same node —
need joint exploration, and that set is small because 93% of transitions are a single
attribute write on their own subtree (§5.4.3). This is partial-order reduction applied to
a page; the existing caps (effects and bytes per transition) remain the backstop, with
overflow counted.

#### 5.4.5.4 Time: record the cycle, drive it natively

A timer-driven carousel is a cycle of states with a period. Exploration already sees the
states (as transitions fired by `setTimeout`/`setInterval` rather than by a handler); the
proposal records the cycle and its period, and the Navigator drives it — which also makes
`prefers-reduced-motion` and pause-on-focus the platform's job rather than each site's.
Clocks, countdowns and "posted 3 minutes ago" are better **recognised** (tier 3 style)
than recorded, since their content is a function of the reader's clock; unrecognised ones
show their converted value and are marked stale.

#### 5.4.5.5 Verification: re-run, do not trust

Every recorded binding is checked before it is published: the converter replays the
recorded responses through the **binding** and through the **page's own code**, and
compares the two DOMs over the bound subtree. A mismatch drops the binding and keeps the
static conversion (§5.4.2's floor: never publish something worse than what was in hand).
This is what makes the approach safe to be wrong: a wrong inference is caught at conversion
time, where it costs a fallback, not at view time, where it would cost a misbehaving page.

#### 5.4.5.6 Totality, and what is measured first

The vocabulary additions are a request template, a field-to-slot mapping, and "stamp this
block once per element of a response array". The last is bounded iteration over data that
is already in hand — it terminates in the length of the response, which the fetcher caps —
so §5's totality discipline holds: no loops over computed conditions, no recursion, no
expression that can run unboundedly. Anything a page does that would need more is
population 3, by definition.

**Before building:**

1. **Coverage, measured on both corpora:** of the interactive elements a page registers,
   how many §5.4.3 already records, and which of the four cases above the rest fall into.
   This decides the order, and whether case 1 is as dominant as assumed.
2. **One prototype, case 1, on one search page** whose results are server-fed: infer the
   binding from the prerender's own responses with perturbation, verify by re-running
   (§5.4.5.5), and report whether it survives a second, independently fetched response.

If (2) cannot infer a binding that survives (1)'s typical page, the proposal is wrong and
should be recorded as refuted here, like §5.4.4.

#### 5.4.5.6a Measured: what exploration covers, and what the rest is (2026-11-05)

Item 1 above, run on both corpora. **The answer reorders the proposal.** Case 1
(server-fed content) was assumed to dominate. It is 1.5% of the elements a page wired:
31 of 2,045, in 5 of 123 documents. The two largest uncovered groups are the recorder's
own limits:
- the 64-probe cap (case 3);
- events it never simulates (part of "other").

**Method.**
- *Interactive element:* every element the page registered a handler on, any event type
  (`addEventListener` or an `on*` slot), plus every native control: form, input other than
  hidden, textarea, select, details, button, contenteditable.
- *Covered:* a §5.4.3 probe recorded a transition for it.
- *Instrumentation:* per element, `prerender` records its events, whether it was probed
  or cut by the cap, and what each probe did. That is: same-origin GETs refused because
  exploration closes the network, cross-origin or non-GET requests refused, beacons,
  timers scheduled and still pending after the probe's drain, and the nodes its effects
  wrote. It is enabled by `PRERENDER_COVERAGE=<file>`, which adds `K` report lines. It is
  observation only: recordings were byte-identical with and without it over batch 2,
  offline (50/50).
- *Classification* (`scripts/navigator-corpus/coverage.py`). Each element gets the first
  rule that applies:
  - **1** the probe tried the network. This rule comes first, even when the probe also
    recorded a transition: that transition is then partial. The first version of the
    script put "covered" first, and that hid all 31 of these;
  - covered;
  - **3** cut by the probe cap;
  - **2** text entry, a form, or only typing events;
  - **4** the probe left timers pending past its drain;
  - *native-only*: a native control with no handler and nothing to submit;
  - *other*, with the reason.

  A search box whose probe fetches therefore counts as case 1, because §5.4.5.2 makes it
  a data binding.
- *Runs:* `PRERENDER_NET=1 PRERENDER_EXPLORE=1 PRERENDER_DEADLINE=180`, as the corpora
  were recorded.
  - **Batch 1:** 85 recordings, but the original HTML was not kept. Their URLs were
    re-fetched on 2026-11-05: 73 served, 12 refused (403, 202 or a network error). So
    batch 1 here is *today's* version of those pages.
  - **Batch 2:** the saved HTML, 50 pages.
- *Raw tables* sit outside the repo, at
  `~/src/navigator-corpus/coverage-2026-11-05/`: per element, per document, the analysis
  output, and the hashes of the two binaries measured.

**Found by measuring, fixed first: the recorder could not see time.** A probe drained the
timers its handler scheduled with *absolute* bounds: 50 callbacks and 500 ms of virtual
time since load. Load drains up to 1,000 callbacks and 5,000 ms. So on any page that used
timers at load, a probe could fire nothing, and every effect behind a `setTimeout` (a
delayed menu, a debounced toggle) went unrecorded. A two-element test page shows it: one
800 ms timer at load turns 1 transition into 0. The fix:
- bounds relative to the probe's start;
- only the timers created during the probe are fired, so the page's own pending timers
  are never credited to a click.

Regression tests (`tests/oracle.rs`) cover both halves. The delayed-effect test fails
against the old drain. Over the corpora the fix moved 39 elements out of case 4: 3
became covered (samsung +1, k8s +2) and 36 drained to no visible effect. Everything below
is **with the fix**. The as-built run is kept beside it (`analysis-asbuilt.txt`): 296
covered, 30 server-fed, 44 time-driven, 915 other, out of 2,044.

**The numbers.** The first three columns count elements with a registered handler. The
last column counts every interactive element, handlers plus native controls.

| | batch 1 (73 docs) | batch 2 (50 docs) | **overall (123)** | of all 6,189 |
|---|---|---|---|---|
| elements with a handler | 1,315 | 730 | **2,045** | |
| **covered** (transition recorded) | 200 (15.2%) | 98 (13.4%) | **298 (14.6%)** | 4.8% |
| 1 server-fed | 30 (2.3%) | 1 (0.1%) | **31 (1.5%)** | 0.5% |
| 2 free-form input | 161 (12.2%) | 14 (1.9%) | **175 (8.6%)** | 287 (4.6%), incl. 112 forms with no handler |
| 3 large state space (probe cap) | 443 (33.7%) | 141 (19.3%) | **584 (28.6%)** | 9.4% |
| 4 time-driven | 4 | 1 | **5 (0.2%)** | 0.1% |
| other | 477 (36.3%) | 475 (65.1%) | **952 (46.6%)** | 15.4% |
| native-only (no handler) | | | | 4,032 (65.1%) |

**Case by case.**

1. **Server-fed: 31 elements (1.5%), in 5 documents.** All 31 probes also changed the DOM,
   so each recorded a *partial* transition: the part that did not need the data.
   - arXiv: 14 checkboxes that fetch;
   - the Rust book: 13 "run" buttons, each a refused POST to play.rust-lang.org;
   - nhs.uk: 2;
   - walgreens and yahoo.jp: 1 each.
   
   Every other probe made no request (642 probes in all). So on these pages, server-fed
   content arrives at **load** (the prerender admitted 1,082 page requests over corpus 1,
   §5.4.1c), not on interaction. §5.4.5.1's binding inference is not where coverage is
   lost. Two blind spots below mean this is a floor rather than a proof of absence:
   delegation, and synthetic events. The 13 Rust-book buttons are not a binding at all;
   they execute code on a server (population 3).
2. **Free-form input: 8.6% of handler elements, and concentrated.** Walgreens alone has
   149, `keydown`/`keyup` on its search and listing controls. Samsung has 11, the
   Guardian 8, NASA 5. Typical shapes:
   - gov.uk's `<form>` on `change,input` and `<input type=search>` on `focus,blur`;
   - the Guardian's `<label>` on `keydown`;
   - 112 forms with no handler at all, which are plain submissions (§5.4.5.2's request
     template).
3. **Large state spaces: 28.6%, and all of it is the probe cap.** The cap (64 probes per
   document) cut 584 elements in only **5 documents**: doc11 255, doc10 170 (W3C specs,
   one `<dfn>` panel each), theverge 79, ynet 62, walgreens 18. Combinations are the
   smaller problem: 433 pairs of *recorded* transitions (counting the partial ones) write
   a common node (§5.4.5.3's interference), in 17 of the 32 documents with transitions. Walgreens has 260 of those
   pairs, NASA 112, El País 23.
4. **Time-driven: 5 elements.** gov.uk's button arms an interval; walgreens and theverge
   schedule timers past the 500 ms probe horizon. Timer carousels with no handler are not
   elements, so this measures time *behind interaction*, not every animated page.

**Other, 952, broken down.**
- **377 load and error handlers** on `<script>`, `<img>` and `<link>`. Registered, but
  not user-interactive.
- **288 probed with no observable effect.** Analytics, `preventDefault`-only links, and
  handlers that read event fields a bare `Event` lacks (`button`, `clientX`, `key`).
- **190 touch and drag**, almost all of them MIT OCW's carousel (197 of its elements are
  in *other*).
- **27 focus and 36 hover or pointer** handlers. Never dispatched: §5.4.3 probes click,
  change, input, submit and toggle only.
- **15 delegating roots in 11 documents** (trello, walgreens, yahoo.jp, ikea, chase):
  React-style roots that register up to 80 event types on one element, and html/body
  click listeners. A probe on the root reaches no component.

**native-only, 4,032.** 1,288 `<button>`, 760 `<button type=button>`, 1,032 checkboxes,
684 `<details>`, and others. The document lane renders checkboxes and `<details>`
natively. The buttons are the delegation blind spot made visible: a button with no
handler of its own that does something is handled by a delegating root. So some of
these 2,048 buttons are uncovered interactions that this method cannot attribute.

**What it says to build, in order.** These replace the earlier assumption that case 1
leads. *Items 1 and 2 are built and re-measured in §5.4.5.6b, which also says what
comes next: generalising a transition across elements that share a handler.*
1. **Make the probe faithful before making it clever.** Dispatch the event kinds pages
   use (pointer, mouse, key and focus, with real `MouseEvent`/`KeyboardEvent` fields), and
   dispatch at the *target* through delegating roots: probe each native control and link
   *under* a root that listens. This is cheap and touches the two largest buckets
   (other, and native-only buttons).
2. **Replace the flat 64-probe cap with a budget per handler shape.** The cap's 584 come
   from 5 documents of many near-identical elements (spec `<dfn>`s, story cards). One
   probe per *distinct handler function* would cover them at a fraction of the cost.
3. **Case 2 (free-form input), §5.4.5.2**, mostly search and filter fields: 8.6%, which
   is the only §5.4.5 case with real weight here.
4. **Case 1 waits.** Re-measure after (1). If delegation was hiding server-fed handlers,
   they show up then. For the §5.4.5.6 item 2 prototype, arXiv's 14 fetching checkboxes
   are the one same-origin-GET candidate in the corpora; nothing else here needs it.
5. **Case 4 is negligible behind interaction** (5 elements). Timer carousels with no
   handler need their own measurement before anyone builds for them.

**Limits of the measurement, stated.**
- Batch 1 is today's pages, not the recorded ones.
- Live network: two documents differed between arms. Doc13 loaded 25 elements instead
  of 70 in one run; four reruns gave 70 and 27 recorded every time, and the rerun was
  used.
- Window- and document-level listeners are outside the element universe.
- The measurement also caught two bugs in the jailed converter's relay (backend §4.7d),
  both fixed and covered by tests. A report line longer than 16 KiB failed the document,
  and so did a 19–179 KB URL a page asked to fetch (tripadvisor, walgreens, old.reddit).
  A child that had exited also turned into a protocol error instead of a timeout
  (theverge, corriere). Those five documents were converted again with the fixed relay.

#### 5.4.5.6b Re-measured after items 1 and 2: gestures, delegation, a per-handler budget (2026-11-05)

Items 1 and 2 of "what it says to build" above, built and measured with the same
instrument. **Covered handler elements rose from 298 to 324 (17.9% → 19.4%), and
probes now reach 3,858 delegated targets, 339 of which recorded a transition.**
What decides the next step is case 3. It is now entirely elements whose handler *was*
probed, on a sibling: 775. The recording does not yet carry what was learned there
over to them.

**What changed in the recorder** (`navigator-prerender`, commits c57631b0, 37c1de60,
4e653a6e, c4bbf2ee).
- *Gestures, not event names.* A probe performs what a reader does, as the sequence a
  browser delivers, with real fields (`button`, `clientX`, `key`, `touches`, …). It
  records the gesture as the transition's `event`:
  - `click`: pointer and mouse down/up, then click. This also reaches handlers that
    listen only for down or up.
  - `dblclick`, `hover`, `drag`, `tap`, `swipe`.
  - `key:Enter`, on non-text elements only. On a text field, keys are §5.4.5.2 input.
  - `focus`.
  - `change` on checkbox, radio and select. The box is checked first, and that
    state change is not counted as the page's effect.
  - `toggle` on an opened `<details>`.
- *Load and error handlers* are never probed. They are now outside both denominators
  (376 elements), in both the before and the after column.
- *Delegation.*
  - A probe on an element with element children is a discovery probe. So is a probe at
    `<body>` for document and window listeners.
  - It records the selectors the handler tests (`closest`, `matches`,
    `classList.contains`), then probes the descendants that match.
  - If the handler tested nothing observable, it probes the interactive descendants.
  - `closest` and `matches` are answered only while exploring. The load pass never had
    them, so adding them there would change artifacts.
- *Budget.* Candidates are grouped by handler source, and native or bound functions by
  object. They are served round-robin: no handler gets a second probe before every
  handler has had a first. The bounds are 64 probes per handler and 256 per document.
- *A probe that spins ends itself.* Found by this measurement: chase.com timed out at
  180 s once delegation reached a link handler that busy-waits on `Date.now()`. The
  converter's clock is fixed, so that loop never ends. While exploring, Boa's loop limit
  (1M iterations per call frame) ends the probe, and that handler gets no further
  probes. 3 probes in the corpora hit it. The load pass has no limit.

**How the bounds were chosen.**
- *Data.* An unbounded run of both corpora. `scripts/navigator-corpus/budget.py`
  replays any pair of bounds from it.
  - 122 documents explore.
  - 943 distinct handlers: median 1 per document, p90 22, max 85 (walgreens).
  - 45,640 candidates.
  - The costliest probe is 14.6 ms.
- *256 per document* is 3× the largest handler count, so every handler in every
  document gets its first probe.
- *Per handler, 8 was tried first and was wrong.*
  - Covered handler elements fell from 298 to 178. Pages whose many elements share one
    handler (NASA, walgreens) dropped from up to 64 probes to 8.
  - Round-robin already gives every handler its turn. The per-handler bound only has to
    stop one handler absorbing the document's total, and it must not be shallower than
    the cap it replaced.
  - So it is 64.
- *Cost at 64/256.*
  - 7,422 probes, 29 s over both corpora.
  - The worst document is 8.3 s (bbcgoodfood). The replay estimated 3.7 s; delegated
    probes on that page cost more than its average.
  - Nothing timed out.

**The numbers.** Handler elements, with load/error-only elements excluded. The
"before" column is the §5.4.5.6a tables, reclassified by today's `coverage.py`. Only
that exclusion changes them, which is why it reads 1,669 rather than 2,045.

| | batch 1 before → after | batch 2 before → after | **overall before → after** |
|---|---|---|---|
| elements with a handler | 1,191 | 478 | **1,669** |
| **covered** | 200 → 214 | 98 → 110 | **298 (17.9%) → 324 (19.4%)** |
| 1 server-fed | 30 → 30 | 1 → 9 | **31 → 39** |
| 2 free-form input | 161 → 13 | 14 → 6 | **175 → 19** |
| 3 large state (budget) | 443 → 583 | 141 → 192 | **584 → 775**, all with their handler probed elsewhere |
| 4 time-driven | 4 → 4 | 1 → 2 | **5 → 6** |
| other | 353 → 347 | 223 → 159 | **576 → 506** |

Over all 5,813 interactive elements (handlers plus native controls), covered went from
298 to 351 (5.1% → 6.0%), and native-only from 4,032 to 3,983.

- **Inside the universe:** 786 native controls were probed as delegated targets and
  127 recorded.
- **Outside it:** delegated targets with no handler that are not controls (`<li>`,
  `<a>`, spans): 3,072 probed, 212 recorded, in 50 documents.
- **Document and window listeners:** found in 85 documents. 12 recorded through
  `<body>` discovery.

Gestures probed, in elements: click 3,297, key:Enter 2,176, tap 607, hover 421, drag
365, focus 216, swipe 209.

**What moved, element by element.** Same 6,185 elements in both runs.
- **Free-form → 3, 145 elements:** walgreens' keydown/keyup handlers. They sit on listing
  controls, not text fields, so they are probed as `key:Enter` now. Walgreens hits the
  per-document bound, so they show up as budget.
- **Other → 3, 75 elements:** hover, focus and touch handlers, now explorable and cut by
  the bound.
- **Native-only → covered, 27 elements:** buttons reached through delegation.
- **Native-only → server-fed, 22 elements.**
- **Other → covered, 17 elements;** **3 → covered, 9 elements.**
- **Covered → not covered, 5 elements:** 3 walgreens elements cut under a handler probed
  elsewhere, 1 ikea now classified server-fed, and 1 walgreens element that recorded
  nothing.

Newly covered, by document: doc13 11, yahoo.jp 7, El País 7, doc24 6, The Hindu 4.

**Case by case, what it says.**
1. **Server-fed: 31 → 39, and delegation did not uncover a hidden population.** The new
   ones are mostly the Guardian: 26 elements whose hover and focus probes ping
   `ophan.theguardian.com`. That is analytics, not data: its refused requests rise from
   989 to 2,889 with exploration on. arXiv's 14 are unchanged. **Case 1 still waits.**
2. **Free-form: 175 → 19.** Most of the old count was key-only handlers on non-text
   elements, which a single Enter now probes. What is left is 11 handlers that react
   only to typing events, 4 forms and 4 text fields (112 more forms have no handler at
   all). §5.4.5.2 is smaller than 5.4.5.6a measured.
3. **Large state: 775, and every one is "represented."** Its handler ran on a sibling,
   and no transition names it. Where they are:
   - doc11 252 and doc10 167: spec `<dfn>`s;
   - walgreens 164;
   - theverge 104;
   - ynet 88.

   Probing deeper buys little: these are near-identical elements. **What converts them
   is generalisation.** One handler's transition, learned on a sibling, would be
   recorded as a template over the elements sharing that handler, with the per-element
   differences (which panel, which id) found by differential probing as in §5.4.5.1.
   That is the next build. It is verified like everything else, by replaying one
   unprobed sibling through the page's own code (§5.4.5.5).
   *Built and measured in §5.4.5.6c: case 3 is 55.*
4. **Time-driven: 6.** Negligible, as before.

**Other, 506:**
- 276 were probed and changed nothing observable;
- 184 are `<img>` with only `dragstart`, cancelled to stop image dragging (OCW). That is
  not a gesture;
- 15 delegating roots whose handlers reached no target that changed anything;
- 13 `mouseout`-only handlers: un-hover, with no hover to undo.

**Safety and regressions, checked:**
- **Artifacts:** with exploration on and off, all 123 recordings are identical apart
  from `transitions` and `interactive_found`.
- **Pipeline:** normalize + `nsg-flag` outputs, drop reports and flag reports are
  byte-identical between the arms, on both corpora.
- **Baseline binary:** the pre-change converter (9ae14a09), run now, writes
  byte-identical non-exploring recordings (0 of 123 differ).
- **The previous run:** against §5.4.5.6a's recordings, 10 documents' normalized output
  differs. That is drift between runs: the pages were fetched live a day apart, and
  doc13 is the known flaky one. The converter code agrees with itself (above).

**Limits, stated.**
- Batch 1 is still today's pages, fetched live.
- The DOM models bubbling, not capture. A listener that only sees non-bubbling events
  through capture (focus on an ancestor) is not delegated.
- "Represented" is not "recorded": the backend can replay only named triggers.
- The worst exploration cost (8.3 s) was measured on this machine, not under a jail.

Raw tables are outside the repo, at `~/src/navigator-corpus/coverage-2026-11-05-r2/`:
- per element: `batch{1,2}-cov.tsv`, with `#stats` and `#fetch` lines;
- `analysis.txt`, and `analysis-before-reclassified.txt`;
- `moves.txt`;
- the unbounded run and `budget-grid.txt`;
- the two superseded runs (8 per handler, and before the loop limit);
- the pipeline outputs;
- the binaries' hashes.

#### 5.4.5.6c Generalised across siblings: one transition per handler, verified on a sample (2026-11-05)

§5.4.5.6b ended with 775 elements in case 3, every one cut under a handler that *had*
been probed, on a sibling. This builds what it said comes next: the recording now
carries what was learned on the siblings over to them.

**The construct: a template.** One handler's transition, recorded once over the elements
that share it (`atrium-navigator-recording/4`, backend §4.5a).
- Its effects are stored once. Each effect's address is one of:
  - a literal path: the same node for every instance (a shared overlay);
  - `$t`: the instance's own trigger (a class toggle on itself, the commonest case);
  - `$N`: the instance's slot N (tab i shows panel i; item i opens its own body).
- Values (`from`, `to`, inserted markup, insert index) are the same for every instance.
  That is what makes it *one* transition.
- Each instance binds a trigger and its slots. Expanding it gives exactly the
  transition a probe on that element records.
- The backend expands templates **at ingest** into ordinary transitions. The §5
  vocabulary and the profile are unchanged: a template is a compression of the
  recording, not a new behaviour, and there is no second replay path.
- A recording without templates stays `/3`, byte for byte.

**How a template is inferred.** Probes are grouped by handler key and gesture, as the
budget already groups them. A group becomes a template only if:
1. **Every probe in it recorded a transition.** One that recorded nothing means the
   handler does not act alike on its elements.
2. **The transitions agree, apart from their addresses.** Same effects, in the same
   order, with equal values.
3. **Each effect's address fits one rule across all the examples.** The rule is
   structural: up `k` ancestors from the trigger, then down a chain of steps. A step
   is a fixed `tag:n`, or `tag:(the trigger's own position at level m) + offset`. If no
   structural rule fits, the rule is a fixed node, provided every example names the
   same one. A structural rule is preferred, because it carries over to a sibling
   elsewhere in the tree.
4. **The rule reproduces every example exactly.**

**Predicting the cut siblings.** For each cut candidate in the group the rule gives the
transition a probe would record. A candidate is not predicted when:
- an address does not resolve;
- an attribute does not currently hold the value its effect starts from (a box already
  on);
- the predicted order is not the order the diff writes.

Gestures whose user action changes state first (`change`, `toggle`) are folded from
their examples but never predicted.

**Verification: probe a sample, and refuse on any disagreement.** The first, middle and
last predicted siblings are probed through the page's own code (§5.4.5.5).
- If any sample records anything but its prediction, **the template is refused**: the
  samples keep their own transitions as ordinary ones, the examples stay per element,
  and the rest stay cut, exactly as before.
- If all three agree, the template replaces its examples' transitions, and the
  predicted siblings are covered (`T` in the coverage table; samples are `v`).
- Groups with no cut siblings are folded too, with no verification needed, since
  every instance was probed.

**A handler that does nothing is not a large state space.** A group whose every probe
recorded *nothing* gets the same treatment. Three cut siblings are sampled, and if
they are quiet too, the rest are `inert` (`I`): their handler is known to do nothing
observable here. If a sample acts, the claim is refused and the sample's own transition
is kept.

**Bounds.**
- At most 96 verification probes per document (32 templates' worth), and 2,048
  predicted instances.
- The backend caps templates at 256, counts instances against `max_transitions`, and
  counts the expanded bytes against `max_total_bytes`, with a note for anything dropped.

**Tests** (`navigator-prerender/tests/templates.rs`, `navigator-backend/tests/templates.rs`):
- tab i → panel i, an accordion, and a self-toggle each become one template. Each
  expands to exactly what probing every element (bounds off) records.
- A disagreeing sample refuses the template.
- Per-element content (a definition panel listing its own references) is not
  templated.
- A sibling already in the target state is not predicted.
- Inert groups are verified, and a sample that acts refuses inertness.
- `/4` only with templates. The recording is reproducible.
- Backend: expansion, replay and going back, refusals, both bounds. The fuzzer gets a
  `/4` seed and must reach the new note.

**Two probe-fidelity fixes, found while breaking down what was left (step 2).**
- **`el.id` was never answered.** The missing-API report counts `element.id` 4,690
  times in 24 documents. A handler written `byId(this.id + '-panel')` computed with
  `"undefined"`, and every element of a group did the *same* wrong thing, which a
  template would have generalised faithfully. It is now answered while exploring.
  Like `closest` and `matches`, the load pass still does not answer it: that changes
  what pages build, and is a separate change with its own corpus diff (listed below;
  done since, §5.4.5.6d).
- **Two report-only counters per probe:** navigations it attempted
  (`location.assign/replace/reload`, all refused), and the host of each request it
  was refused. They separate "did nothing" from "did something exploration refuses",
  and data from pings.

**The numbers.** Handler elements, load/error-only excluded, as in §5.4.5.6b. The
"before" column is §5.4.5.6b's tables. The pages are the same: the control arm
written today is byte-identical to the r2 binary's, run today (0 of 123 differ).

| | batch 1 before → after | batch 2 before → after | **overall before → after** |
|---|---|---|---|
| elements with a handler | 1,191 | 478 | **1,669** |
| **covered** | 214 → 633 | 110 → 121 | **324 (19.4%) → 754 (45.2%)** |
| 1 server-fed | 30 → 30 | 9 → 9 | **39 → 39** |
| 2 free-form input | 13 → 15 | 6 → 6 | **19 → 21** |
| 3 large state (budget) | 583 → 1 | 192 → 54 | **775 → 55** |
| 4 time-driven | 4 → 4 | 2 → 2 | **6 → 6** |
| other | 347 → 508 | 159 → 286 | **506 → 794** |

Over all 5,813 interactive elements, covered went from 351 to 780 (6.0% → 13.4%).

**What moved, element by element** (`moves.txt`; the same elements in both runs):
- **3 → covered, 419:** doc11 252 and doc10 167, the spec `<dfn>`s. 413 of them are
  predicted instances and 6 are samples. They come from 6 templates; none was refused.
- **3 → other, 299:** walgreens 161, theverge 104, ynet 34. Every probe of their handler
  recorded nothing, and so did a sample of the rest. They are inert, not a large state
  space: 139 of walgreens' are `keyup` on navigation `<li>`s, which Enter does not
  visibly change.
- **3 → free-form, 2** (walgreens, sampled).
- **other → covered, 12, and covered → other or native-only, 2:** El País, from the
  `el.id` fix (below).
- **native-only → server-fed, 25:** Guardian buttons, delegated targets whose root's
  handler pings `ophan.theguardian.com`. A predicted or inert instance is classified as
  its siblings were, so it takes their requests too.

**Case 3 is 55, and none of it is the budget's fault now.** ynet 54 and walgreens 1 are
groups whose probes disagreed: some recorded a transition and some did not, so neither
a template nor inertness can be claimed. Probing deeper would find out which elements
are which.

**Server-fed, 39, by where the refused requests went** (the new host counter):
- **data, 18:** arXiv 14 (same-origin GET, the §5.4.5.6 item 2 candidate), nhs.uk 2,
  yahoo.jp 1, and IKEA's search API 1;
- **remote execution, 13:** the Rust book's "run" buttons, a POST to play.rust-lang.org
  (population 3);
- **analytics, 8:** the Guardian 6 (`ophan`), Python docs 2 (`analytics.python.org`).

So case 1 is 18 elements in 4 documents, and still waits.

**Other, 794, by cause:**
- **No observable effect, 564.** Of these, 356 are verified inert (above) and 208 were
  probed. Split by what the probe saw:
  - 188 links, whose effect is the navigation exploration does not perform;
  - 89 submit controls, whose effect is the form submission;
  - 34 that scheduled timers which changed nothing;
  - 253 with nothing observable at all. Most are `<li>` (146), `<div>` (56) and
    `<button>` (26).

  No probe in either corpus called `location.assign`, `replace` or `reload`. Where a
  handler navigates, it does so by the link's default action or by assigning `href`.
- **184 `<img>` with only `dragstart`** (MIT OCW), cancelled to stop image dragging. Not
  a gesture.
- **15 delegating roots** whose targets changed nothing.
- **31 unsimulated:** 13 `mouseout`-only, 7 `invalid`, the rest one-offs (custom events,
  media events, `transitionend`).

**Safety and regressions, checked:**
- **Artifacts:** exploring and non-exploring recordings are identical apart from
  `transitions`, `templates`, `format` and `interactive_found`, on 123/123. 14
  recordings are `/4`, and every control recording is `/3`.
- **Baseline binary:** the §5.4.5.6b converter (c4bbf2ee), run today, writes
  byte-identical non-exploring recordings (0 of 123 differ).
- **Pipeline:** normalize + `nsg-flag` outputs, drop reports and flag reports are
  byte-identical between the arms. They are also identical to §5.4.5.6b's, on both
  corpora.
- **The oracle:** c4bbf2ee, exploring, run today.
  - Every one of its 814 transitions is in this recording's expansion, except 19 in
    doc09, doc24 and El País.
  - A build with only the `el.id` read turned off reproduces the oracle exactly on those
    three documents. So all 19 are the `id` fix: `data-id="undefined"` became the real
    id, and `"js sidebar-visible undefined"` became the real theme.
  - The expansion holds 2,362 transitions against the oracle's 814. The transition and
    template tables together are 729 KB against 731 KB.
- **Replay:** the backend's real-recording tests (`tests/roundtrip.rs`) ingest all 123.
  Every expanded transition applies, reverses and goes forward exactly, except 6
  failures: gov.uk 2, chinadaily 2, yahoo.jp 1 and ynet 1. The oracle's recordings
  give the same 6, so they predate this change. They are listed for their own fix.
  - *Fixed 2026-11-05,* all in replay, not in the recordings. chinadaily: the backend
    had its own path walk, and it matched element tags only, so the `node:N` step that
    `node_path` writes for a text node never resolved. Paths now resolve through
    `Dom::resolve_path`, the inverse of `node_path` in the same crate. ynet and yahoo.jp:
    the converter addresses every effect against the document *before* the step, but
    replay resolved each effect after the earlier ones had run. ynet renames two ids, and
    its undo named them by the old ids. yahoo.jp inserts a `<div>` ahead of the `div:1`
    that a later effect addresses. Replay now resolves every address first, then applies
    the step: attributes, then removals, then inserts in ascending index. The undo is
    addressed against the document it will be applied to. gov.uk: undoing a removed
    `hidden` appended it, which gives the same meaning in different bytes, so the undo
    now puts it back where it was.
  - A seventh failure was hidden behind them: the broker offered doc13's sidebar toggle,
    whose probe dropped 82 effects, and then refused it as incomplete. It now offers only
    complete, anchored steps.
  - Found while reading the converter's side: an insert's index counted inserted
    whitespace text that the recording does not carry, which put later inserts one place
    too far right.
  - After the fix, all 2,241 anchored, complete transitions apply, reverse, go forward
    again and drive the broker. The 105 unanchored and 16 incomplete ones are skipped by
    design. r2's 693 are also clean. The cases are in
    `navigator-backend/tests/replay_contract.rs`; 7 of its 8 fail on the code before.

**Cost.**
- 7,422 budgeted probes (the same as before) and 469 verification probes.
- 30.9 s of exploration over both corpora, against 29.3 s for the oracle run beside it.
- The worst document is 8.8 s (bbcgoodfood, unchanged).
- 29 templates were kept and none refused. 1,511 instances were predicted (605 of them
  delegated targets outside the universe).
- 31,832 cut candidates were verified inert, nearly all delegated targets.

**Limits, stated.**
- Batch 1 is still today's pages, fetched live.
- Three samples verify a template; they do not prove it. A sibling that differs from
  its neighbours in some way the structure does not show is predicted wrongly. The
  static checks (address resolves, start value matches, diff order) catch only the
  visible kinds.
- `el.id` is answered while exploring only. **Next:** answer it at load too, with its
  own corpus diff (24 documents read it). *Done: §5.4.5.6d.*
- Case 3's remaining 55 are mixed groups. A template per *behaviour class* within a
  handler group (split the siblings by whether their probe recorded) would take them.
  It needs a structural predicate for the split, and has not been tried.

Raw tables are outside the repo, at `~/src/navigator-corpus/coverage-2026-11-05-r3/`:
- `batch{1,2}-cov.tsv` (now with the navigation and refused-host columns);
- `analysis.txt` and `analysis-r2-reclassified.txt`;
- `moves.txt`;
- the oracle run, `oracle-r2bin/`;
- the pipeline outputs;
- two superseded runs: `step1/`, before the step 2 fixes, and `second-742b96/`, before
  the two counters.

#### 5.4.5.6d `el.id` answered at load too (2026-11-05)

§5.4.5.6c answered `el.id` while exploring only, so that its effect on what pages
*build* could be measured on its own. This is that measurement.

**The change.** `probe_get` now answers `id` on any element wrapper at load and while
exploring, from the `id` attribute. Writes are unchanged: `el.id = x` still sets a
script property that never reaches the attribute. Reflecting writes is a separate
change, with its own diff.

**Instrument.**
- Both corpora, exploring, as the corpora were recorded, with one change: offline.
  `PRERENDER_OFFLINE=1` answers every fetch from the fetch caches and fails a miss.
  With the network open, a miss goes back out to the live site, and failures are never
  cached, so a re-run can be fed different bytes.
- Two binaries that differ in this one change: before (`f2fc5755…`, main at af478f30
  plus the offline switch) and after (`e31a5b17…`).
- Each binary ran twice, in separate processes. Then navigator-normalize and nsg-flag
  ran over every run.
- *Noise floor:* each binary reproduces itself on 123/123 recordings, normalized
  documents and flag rows. So every difference below is the change.

**What changed.** Of 123 recordings, 7 differ (wikihow is in both corpora). The
normalized output differs in 6 documents. nsg-flag's rows are identical in all 123.
After the change, `element.id` no longer appears in the missing-API report; before, it
was there, 286 hits in 10 batch 2 documents.
- **wikihow** (both corpora): 28 step images go from `id="img-undefined"` to their real
  ids (`img-mvid-83d286d09e`, …). Better: the ids were duplicates before.
- **go.dev Effective Go:** 214 table-of-contents links go from `href="#undefined"` to
  the headings they name (`#introduction`, …). Better: every one was dead before.
- **doc13** (Rust book, mdbook): 12 sidebar header links get their real anchors.
  Better. But the `current-header` highlight moves from the first header to "Return
  Values and Scope". mdbook marks the last header whose `getBoundingClientRect().top`
  is at or above a threshold. Our rects are all at 0, so the last header wins, where a
  browser at scroll 0 marks the first. Before, every href was `#undefined`, so the
  lookup found the first link: right, by accident. The raster differs in one 14-pixel
  band (two links' colour), at the same height. The fault is the rect stub; it is
  listed below.
- **walgreens:** a "See more" button's `aria-controls` names `.wag-groupicon-4140`,
  which two elements carry as their id. Before, it named an id no element had.
- **El País:** a load-time handler now writes `style="display: none"` on 7 classified
  lists itself. The probe used to write it, so the same effects leave its transitions
  (16 transitions, as before). 52 interactive elements found, against 43. Normalized
  output identical.
- **The Hindu:** one load-time script stopped failing (`scripts_failed` 1 → 0), and it
  shows one JW Player container (`display: none` → `block`). The container is empty:
  the normalized tree is the same 3,495 tokens, and the raster is byte-identical.

**Geometry.** No document's layout moved. The flag rows are identical; the two rasters
that could move are identical (The Hindu) or recoloured only (doc13); the rest differ
in `id`, `href` and `aria-*` values only. So there was nothing for the Chrome oracle to
arbitrate, and it was not run.

**Kept:** six documents improve, and the one detail that got worse (doc13's highlight)
was right only by accident before.

**Next:**
- `getBoundingClientRect` reports 0 for everything, so "last header above the line"
  logic picks the last header. Answering it from the normalizer's layout is the real
  fix; a first-match rule would be a guess.
- Reflect `el.id` writes into the attribute.

Raw data, outside the repo, at `~/src/navigator-corpus/eid-2026-11-05/`: `bin/` (both
binaries), `before-{1,2}/` and `after-{1,2}/` (recordings, blobs, reports, coverage,
normalize and flag outputs), and `run.sh`, `pipeline.sh` and `compare.py`.

#### 5.4.5.6e Layout geometry for the state recorder (2026-11-05: investigated, then built)

**What the converter answers today.** These are fixed answers, not zeros:
- `getBoundingClientRect()` gives x = y = top = left = 0, width = right = 1280 and
  height = bottom = 600, for every element.
- `offsetWidth`, `clientWidth` and `scrollWidth` are 1280; the three `*Height` metrics
  are 600.
- `offsetTop`, `offsetLeft`, `scrollTop` and `scrollLeft` are 0.
- `IntersectionObserver` and `ResizeObserver` accept `observe()` and never deliver.
- `innerWidth` × `innerHeight` is 1280 × 600.

Every element therefore claims to be 1280 × 600 and at the origin: visible, in the
viewport, never scrolled, and all the same size.

**Measured.**
- *Static:* 92 of 124 corpus pages ship a script that mentions one of these APIs
  (inline scripts plus 1,094 external scripts from the fetch caches). This is a
  mention, not a call.
- *Dynamic:* both corpora were converted offline with a per-read log, a temporary
  patch that is not committed. Exploration was on and the deadline 180 s. Of 126
  documents, **37 read geometry during conversion**: 7,621 reads, p50 5 per reading
  document, p90 478, max 3,971. **15 register a geometry observer** (73 `observe()`
  calls), and those observers never fire.

| API | reads | documents |
|---|---|---|
| `getBoundingClientRect` | 4,274 | 9 |
| `offsetWidth` | 2,414 | 13 |
| `offsetHeight` | 568 | 14 |
| `clientHeight` | 106 | 18 |
| `scrollHeight` | 82 | 12 |
| `scrollTop` | 54 | 10 |
| `clientWidth` | 52 | 14 |
| `scrollLeft` / `scrollWidth` / `offsetTop` / `offsetLeft` | 43 / 21 / 6 / 1 | 5 / 4 / 6 / 1 |

**What it is used for, and what goes wrong.** Six patterns cover the volume.

- **Lazy loading by visibility** (The Hindu: 1,800 `offsetWidth` reads on
  `img.lazy`). The fiction *helps* here. Every image looks visible, so the loader swaps
  in real sources: 86 of 118 placeholders became real images in the recording. Loaders
  built on `IntersectionObserver` get no callback, so their images stay placeholders.
  Those 15 documents are where that happens.
- **Equal-height cards** (Samsung). The script reads each card as 600 px tall and
  writes `min-height: 600px` onto card contents **and their headlines**: 9 of the 11
  nominal values the corpora's recordings gained in `style` attributes. These are tall
  blank gaps. nsg-flag cannot see them, because nothing overlaps.
- **Scroll-spy** (doc13, mdbook). The script marks the last header whose top is at or
  above a line, and every top is 0, so it picks the last header ("Return Values and
  Scope"). A browser at scroll 0 marks the first (§5.4.5.6d).
- **Carousels and scrollers** (Yahoo JP: `scrollLeft`/`scrollTop`/`clientWidth` on
  carousel tracks). Arrow states and offsets are computed from 0 and 1280. No damage
  was found in the recordings.
- **Page-shape probes** (Python docs: `scrollHeight` vs `clientHeight` on `html` and
  `body`; Trello: 474 `offsetWidth`/`offsetHeight` reads on spans, a font-load probe).
  The script decides something, the document is unchanged.
- **Per-icon polling** (Walgreens: 3,956 `getBoundingClientRect` on SVG `<use>`). Pure
  cost, no effect found.

Writes of measured values back into the document are rare: +11 `600px` values in 3
documents and no `1280px`. The harm is mostly in *decisions* (which header, which
image, whether to equalise), not in copied numbers.

**Options.**

1. **Keep the nominal answers** (today). Deterministic and free. It helps visibility
   loaders and harms equalisers and scroll-spies. Its cost is invisible to the flagger.
2. **Zeros, a "no layout" answer.** Deterministic and free, but worse. Scripts read
   0 × 0 as hidden, and collapse or skip the element; the converter's comments already
   record this. It would also undo The Hindu's 86 images. Not recommended.
3. **Answer from our own layout, once.** At the first geometry read (or at load end),
   normalize the *current* DOM and lay it out with navigator-render at the conversion
   viewport. Answer every read from that snapshot until exploration starts, and deliver
   one initial `IntersectionObserver`/`ResizeObserver` record from it, as a browser does.
   - Cost: one normalize plus render. Measured 0.2–3.4 s per document (Samsung 1.9 s,
     edX 3.4 s, The Hindu 1.2 s, doc13 0.2 s), against the 180 s deadline.
   - Determinism is kept: the normalizer and renderer are pure functions of the
     document, the pinned fonts and the viewport, as recordings already must be (threat
     model §2.6). The snapshot's digest goes in the recording, so a re-run can check it.
   - Fixes: scroll-spy (real tops), IO-based lazy loading (real intersections) and
     equalisers (real heights, at the *conversion* viewport).
   - Limits:
     - A stale snapshot: a script that writes then reads gets pre-write geometry. Per
       read relayout is unaffordable, since Walgreens alone reads 3,956 times.
     - A renderer gap returns a wrong number instead of an obviously fictional one.
     - The conversion viewport is not the reader's. An equaliser bakes heights for
       one width, so heights written into `style` should still be refused (option 4).
4. **Refuse geometry-derived writes.** Taint the values the geometry getters return,
   and drop a script's write of `style.width`/`height`/`min-*`/`max-*`/`top`/`left`/
   `transform` whose value came from one. Cheap and deterministic, and it removes the
   Samsung class at any viewport. It does nothing for decisions (scroll-spy, observers).
   Taint through string concatenation needs a wrapper number type or a sentinel value,
   so this is the fiddliest to make exact.

**Recommendation:** 3 and 4 together. One snapshot answers reads and observers; tainted
writes are refused, because whatever the snapshot says is still the wrong viewport for
the reader. Measure before building, as §5.4.5.6a did:
- the 37 + 15 documents' recordings, normalized output and flag rows, before and after;
- the Chrome oracle on every document whose layout moves;
- reproduction (123/123) re-checked with the snapshot on.

The per-read log patch, both logs and the scan scripts are outside the repo, in
`~/src/navigator-corpus/geo-2026-11-05/`.

**Built (2026-11-05): options 3 and 4, and a reader's sweep the measurement showed was
needed.** `navigator-prerender/src/geometry.rs`, the geometry natives in `boa_impl.rs`, and
`navigator-render`'s `layout_geometry`.

- *The layout.* The first geometry read serializes the live DOM with
  `data-atrium-geo="<handle>"` on every element (`Dom::serialize_marked`, the one
  serializer with a marker threaded through). It collects the sheets and image sizes the
  recording would carry, normalizes with `navigator-normalize`, and lays out with
  `navigator-render`, which records every box and keys it by the marker. These are the
  lane's own stages, not a second layout engine. Node positions do not survive
  normalization (row-reverse reorders, whitespace is stripped), but a marker attribute
  does.
  - *Viewport:* 1280 × 600, which is what the converter already tells scripts
    (`innerWidth`, `innerHeight`). The lane renders at 800 px; see the refusal below.
  - *What the renderer records:* each block-level and atomic box's border box, borders
    and padding, and whether it is positioned. Inline boxes get one fragment per line,
    carried on the item the way link spans already were. A trial layout (`measure`, as
    a flex item gets) rolls its boxes back, so only real placements survive (a test
    checks a flex item's x).
  - *Geometry is opt-in:* a render without it records nothing. All 133 corpus scenes
    and normalized documents are byte-identical with this renderer, and WPT is 2,083
    passes before and after, with no test changing result.
- *Answered from it:*
  - `getBoundingClientRect`, `getClientRects` (new), `offsetWidth/Height`,
    `clientWidth/Height` (the padding box; 0 for an inline box), and
    `clientTop/Left` (new).
  - `scrollWidth/Height`: the padding box, or further where descendants reach.
  - `offsetTop/Left` and `offsetParent` (new): the CSSOM View rules. The parent is the
    nearest positioned ancestor, or `td`/`th`/`table` for a static element, else
    `body`; positions are relative to its padding edge, or to the page when it is
    `body`.
  - For the root, `documentElement.clientWidth/Height` is the viewport and its scroll
    size is the document's.
  - An element with no box (`display: none`, detached, in `<head>`) answers zeros, as a
    browser does.
  - The old nominal box survives only when no layout is possible: the engine has
    geometry off (`PRERENDER_GEOMETRY=0`, kept for A/B), or no layout succeeded.
    Corpus failures: 0.
- *When it lays out again.* A read lays out again only after a mutation; the cache key
  is the DOM's mutation count. A probe's restore swaps the old DOM, with its old count,
  back in, so the snapshot is restored with it (a test pins this). There are at most
  `MAX_LAYOUTS` = 8 layouts per conversion, by count. After that the last layout
  answers, and the read is counted `stale`.
- *Observers.* Each (IntersectionObserver or ResizeObserver, target) pair gets the
  record a browser delivers first, in a task after `observe()`:
  - IntersectionObserver: the viewport at the scroll position (or the `root`
    element's box), grown by `rootMargin`. Edge-adjacent counts. Clipping by
    overflow ancestors is not modelled.
  - ResizeObserver: the content box. A 0 × 0 box gets no record.
  - PerformanceObserver still never delivers.
- *Option 4, the refusal.* A style write of `width`, `height` or their `min-`/`max-`
  forms (property assignment or `setProperty`) is refused and counted when its value
  is a nonzero px length equal to a size a getter returned (exactly, or rounded to
  whole px). Such a height was baked at 1280 px, and the document is read at other
  widths.
  - Rather than taint tracking, the rule is exact matching. It does not catch derived
    values (a sum, an added margin), positions, `cssText` or zero (collapsing is not
    baking).
  - Refused on the corpora: 18 writes in 3 documents. Samsung's 9 (`min-height:
    85px`, the equaliser on headlines), ynet's 7 (`width: 687px`), Walgreens' 2.
- *Found while measuring, and built (★ the reader's sweep).* Honest geometry makes lazy
  loaders honest. At scroll 0 only the first screen is in view: Samsung's
  rect-on-scroll loader filled 9 of 63 images, and an IntersectionObserver loader
  fills one screen.
  - The nominal box had loaded everything by accident, because every element claimed
    to be on screen. A recording is read, and its reader scrolls.
  - So after load, a page that can notice a scroll (a `scroll` listener, or an
    IntersectionObserver with targets) is scrolled as a reader would scroll it. It
    goes down one viewport at a time, at most 40 steps. Each step dispatches `scroll`,
    re-evaluates the IntersectionObservers (delivering only changes), delivers
    mutations and drains timers with a probe's relative bounds. Then it returns to the
    top, where the recording is taken.
  - What the page did on the way stays (loaded images, revealed sections). What
    depends on the position (a scroll-spy mark, a sticky header's class) is recomputed
    by the page at scroll 0. `PRERENDER_SCROLL=0` turns it off. 23 documents were
    swept, 501 steps in all.
- *Also found and fixed while measuring:*
  - (a) An image with no `src` (a lazy placeholder) had no box. The normalizer drops
    an unsized image from the document, which is right for the artifact, so
    Samsung's `offsetWidth || getClientRects().length` check never passed. For the
    layout only, unsized images are now declared 0 × 0 with no ratio. That is what
    Chrome lays them out as (measured: 300 × 0 under `width: 100%`, 0 × 0 bare, one
    client rect each).
  - (a, continued) The renderer had treated a natural dimension of 0 as absent, which
    fell back to the 300 × 150 default object size. A 0 is now a dimension (CSS Images
    3 §5.1). Corpus scenes are unchanged, since no recording declares 0.
  - (b) Layout-time image fetches went through the relay a second time and pushed The
    Verge past the 128 MiB per-conversion fetch budget (37 images refused). One memo
    per conversion now serves layouts and the final collection, so each URL crosses
    the fetcher once. Relay bytes per conversion are back to the before figures (max
    28.0 and 79.3 MB).
- *Tests.* Converter `tests/geometry.rs` (13): scroll-spy, real rects and zeros, inline
  rects per line, offsetParent, the equaliser refused (and a non-measured write and
  zero applied), IntersectionObserver initial records with `rootMargin` and
  `display: none`, ResizeObserver content box, relayout budget and staleness,
  determinism, the A/B switch, the sweep (rect and IntersectionObserver loaders, ending
  at the top), no sweep without listeners, and a source-less image's box. The old
  nominal test now pins `nominal_geometry`. Renderer `tests/geometry.rs` (7). The
  converter suite is 386/0 and the renderer suite 154/0.

**Measured (2026-11-05, one machine, both corpora offline, 123 pages, exploring).**
- *Method.* Main's converter (built in a scratch worktree) against this one. The fetch
  cache was first warmed by one online run of the new converter, which fetched only
  URLs the old one never asked for (The Hindu's right-sized image variants, images
  loaded on the way down), as `record.sh` would. Then both arms ran offline, with
  `navigator-normalize` and `nsg-flag` over each.
- *Reproducibility (the c92d16ea check):* two offline runs of the new converter are
  **123/123 identical** in recording, normalized document and flag row.
- *Who reads:* 38 documents laid out (0 failed), 138 layouts. Reads: 2,360 from a
  current layout, 7,796 stale (Walgreens' icon polling and Samsung after their 8
  layouts). Observer records: 94 IntersectionObserver, 17 ResizeObserver.
- *What changed:* 13 of 123 recordings. Flag totals are unchanged in both batches
  (batch 1: OVERLAP 4, SLICED 8, TEXTLOSS 4, OFFCANVAS 5; batch 2: 12, 8, 3, 13). Only
  Samsung's, Walgreens' and ynet's rows move, and only in coverage.

  | document | before | after |
  |---|---|---|
  | **doc13** (mdbook) | current header "Return Values and Scope" (the last) | "What Is Ownership?" (the first, as a browser at scroll 0); transitions 40 → 51 |
  | **Samsung** | 9 × `min-height: 600px` on headlines; 63 images | 0 (9 measured writes refused); 38 images; page 8,576 → 7,014 px here, and in Chrome on the normalized document 6,648 → 4,940 px |
  | **The Hindu** | 96 images (the 1200-px variant of each) | 101 (the variant for each image's real width: 230–660 px), all its lazy images still loaded |
  | Walgreens | one `600px` height | none (2 writes refused) |
  | IKEA | 1 transition | 2 |
  | ynet | | 44 more words; 7 writes refused |
  | LinkedIn | 7 images | 5 (two in a 564 px modal header) |

  Samsung's other 25 images are ones a browser at 1280 px does not load at rest
  either: carousel slides outside the visible area (14), inactive tab panes (5), and
  mobile and video variants. The carousel arrows and tabs are transitions; their
  images are not fetched until a reader takes one. That is a cost the old fiction
  hid, written down rather than faked back.
- *Chrome.* Chrome was run on the normalized document (images mapped to blobs,
  standards mode, border-box, Plex). Iframes were stripped, because their YouTube and
  `javascript:` sources kept `load` from firing. It confirms Samsung's gap removal
  (6,648 → 4,940 px). The Hindu still never fires `load` in this harness, so its gain
  is checked by images, not pixels. doc13's change is a class, and its page height is
  14,558 px in both arms.
- *Cost.* Layouts take 0.05–9.4 s each (node_fs 9.4 s for one layout, of
  `MAX_LAYOUTS` per document). In total: 70.8 s over batch 1 and 68.1 s over batch 2.
  Conversion wall per page:

  | | before p50 / p90 / max | after p50 / p90 / max | total |
  |---|---|---|---|
  | batch 1 (73) | 0.30 / 2.2 / 10.2 s | 0.40 / 4.9 / 20.1 s | 63 → 135 s |
  | batch 2 (50) | 0.21 / 1.5 / 4.7 s | 0.55 / 4.6 / 26.7 s | 30 → 98 s |

  The slowest page (ynet, 22 s of layouts) is 15% of the 180 s deadline. The deadline
  is wall-clock, which the threat model already lists as open (§2.6), and layout makes
  it matter more on a slow machine.
- *Open:*
  - Re-decoding or re-laying out for the reader's own viewport. Geometry is answered
    at 1280 × 600, and that is why size writes are refused.
  - Transforms in rects.
  - IntersectionObserver clipping by overflow ancestors.
  - The snapshot digest in the recording (proposed above; not added, to keep the
    format).
  - A faster layout for repeat reads: the 8-layout budget is what bounds Walgreens'
    3,956 reads.

#### 5.4.5.7 Time to first display (2026-11-05: measured, then built)

**The goal, revised by the user on 2026-11-05:** *"let's abandon the 1s goal..its too
tight..we can live with a few seconds..the added bonus is the security".* The goal is now
**a first display within a few seconds**, which the cold p90 already meets (3.2–3.8 s from
the main document's arrival, cuts A and B below), and no conversion makes a reader wait
for the whole pipeline. The further speed cuts are **parked, not scheduled**: cold image
headers, HTTP/2 or a stylesheet cache, normalize's remainder, and converting ahead. They
are taken up only if a page is unusably slow. The measurements below stay as the record
of where the time goes.

*The original goal* was a first display in under 1 s for 90% of pages. "First display" is the moment a reader can
see the document: the main document fetched, a recording converted, then normalized and
rendered. Before this section, the only recording was the whole one: scripts, layouts and
exploration all ran before anything could be shown.

**Method.**
- *The instrument* (a1566441, `timing.rs`). The converter times each phase as exclusive
  spans on a stack, so a fetch inside a layout inside a script is charged once, to the
  fetch. Each phase is also recorded inclusively. Every fetch is logged with its kind,
  start, duration and bytes. The driver's relay records curl's own DNS, TCP, TLS,
  first-byte and total time for each request. The driver times the display path in-process:
  recording ingest, normalize, and render at the 800 px reading width (fonts loaded once,
  ~16 ms, not counted). Output is `PRERENDER_TIMING`, one line per document.
  `PRERENDER_TIER1=1` converts without scripts.
- *The pages.* Both corpora, 122 pages. Each main document was fetched live once with
  curl, timed (`main.tsv`). Four pages appear in both batches and are counted in each.
- *The arms.* All on one machine (M4 Max), on its home network, sequential, one document
  at a time:
  - *cold:* online, every cache empty;
  - *warm:* offline, from the cold arm's cache.
  - Step 1 used the curl measurement fetcher: fetchd ran only on FreeBSD then. It has
    the same shape as fetchd (one request at a time, a new connection per request).
  - The after arms ran through the real fetchd, unconfined on macOS (`--unconfined-for-
    testing`, step 4): the same serve loop, protocol, address rule and pins.
- *Two disturbances, stated.*
  - The network dropped from ~12:40 to 13:10 IST. That made batch 2 of the cold
    whole-pipeline arm, and both cold after arms, fetch almost nothing; they "finished"
    in 39 s. Those runs were repeated into new directories with a fresh cache, and only
    the clean runs are reported. ★ An arm that finishes fast is a finding about the
    network before it is one about the converter. Per-arm fetch-failure counts are now
    checked before any arm is read.
  - Another workload (~5 cores) started at 16:40, after every arm had finished. It
    overlaps only the normalizer timings at the end, which are therefore given as
    interleaved ratios.
- Data, scripts (`analyze.py`, `final.py`, `parts.py`) and the binaries' hashes are in
  `~/src/navigator-corpus/ttfd-2026-11-05/`, outside the repo.

**Measured: where a cold conversion's time goes** (the whole pipeline, as it was; ms per
document; 114 converted, 8 timed out at the 180 s deadline).

| phase | docs > 0 | p50 | p90 | p99 | top |
|---|---|---|---|---|---|
| main document fetch | 114 | 815 | 2,889 | 9,554 | asahi 10.4 s, th.wikipedia 9.8 s, ynet 8.2 s |
| · DNS / TCP / TLS (p50) | | 104 / 71 / 95 | 773 / 263 / 305 | | |
| html parse | 113 | 2.3 | 12 | 33 | |
| script fetch | 98 | 2,406 | 19,083 | 32,442 | craigslist 42 s, pinterest 33 s |
| module fetch + mirror | 23 | 0 | 1,048 | 29,273 | mastodon 67 s |
| subresource fetch (sheets, images) | 79 | 1,908 | 31,448 | 118,137 | ar.wikipedia 177 s, nasa 124 s |
| page fetch (the page's own XHR/fetch) | 17 | 0 | 517 | 16,996 | |
| layout fetch (images for geometry) | 33 | 0 | 13,859 | 54,786 | simonwillison 57 s |
| **all fetches** | 112 | **11,940** | **61,855** | **140,420** | |
| js parse + compile | 101 | 24 | 730 | 2,635 | doc21 2.8 s, walgreens 2.7 s |
| js execute (classic) | 97 | 4.9 | 100 | 3,158 | npr 3.6 s, asahi 3.6 s |
| js modules / events + timers | | 0 / 0.7 | 21 / 105 | 872 / 482 | |
| layout passes (geometry) | 33 | 0 | 2,420 | 19,132 | ynet 22 s, ikea 19 s |
| exploration (inclusive) | 100 | 1.4 | 527 | 12,077 | ocw.mit.edu 34 s |
| serialize + emit | 111 | 1.9 | 7 | 14 | |
| **conversion** | 114 | **13,517** | **66,196** | **142,213** | ar.wikipedia 178 s |
| display: normalize | 114 | 302 | 2,915 | 12,483 | coreutils manual 25.6 s, ko.wikipedia 13 s |
| display: render (800 px) | 114 | 39 | 230 | 978 | |

Warm (offline from that cache), the conversion is 404 / 4,582 / 20,315 ms (p50 / p90 /
p99). Its tail is layouts (ynet 22 s), exploration (bbcgoodfood 8.6 s) and JS parse +
compile (p90 728 ms). Display normalize, at p90 3.0 s, is then the largest single phase.

**Findings.**
1. ★★ **Fetches were strictly serial, one connection each.** The intervals of 0 of 114
   cold conversions overlapped (union = sum: 2,754 s). Over 4,580 network requests,
   connection setup (DNS + TCP + TLS) was 1,147 s of 2,709 s fetching. 962 s of that was
   to a host the same conversion had already connected to. Fetches are ~90% of a cold
   conversion. **All 8 timeouts are fetch-bound.** Serial subresource fetches alone took
   19–111 s on them, even with no script run (the tier-1 arm). corriere passed 180 s
   with its images alone.
2. **Tier 1 is not cheap cold.** With no script run, time to tier 1 is p50 8.5 s and
   p90 45 s, because the parsed document's stylesheets and images are fetched serially.
   Warm, it is p50 1.6 s and p90 7.0 s; that is normalize.
3. **The network floor from here.** The main document alone is under 1 s for 75 of 122
   pages (61%), at p50 814 ms and p90 2.9 s; DNS alone has a p90 of 773 ms. So the goal
   cannot be met for 90% of pages on this connection, however fast the rest is. Every
   result below is therefore also given **from the main document's arrival**, which is
   the part the Navigator owns.
4. **Normalize is the display path's cost**, and it had a quadratic: the coreutils manual
   took 25.6 s. That was fixed while measuring (below).
5. Scripts are the smaller cost. JS parse + compile has p90 0.7 s and execution p90
   0.1 s; their tails (asahi, npr, walgreens: 2.7–3.6 s) are per-library. A bytecode cache
   for common libraries would help those few pages, not the distribution.

**Built (2026-11-05): steps 2–4, and the normalizer's quadratic.**

*Step 2, progressive display.* The converter delivers the tier-1 conversion first, and the
broker shows it; the scripted conversion replaces it later, if it is worth replacing.
- *Converter* (`convert_progressive`, `PRERENDER_PROGRESSIVE=1`, serve header
  `progressive=1`). The child sends `FIRST` (the tier-1 recording: parsed, its stylesheets
  and image sizes collected, no script fetched or run, tier 1 with the reason "first
  display: scripts not run yet") the moment it exists. It then runs the scripted
  conversion on the same fetch memo, so no stylesheet or image is pulled twice. The driver
  relay accepts the frames in order only, and `drive_streaming` hands each to its caller
  as it arrives.
- *Broker* (`progressive.rs`; `Upgrade`, `Swap`, `Attach`).
  - `Upgrade { session, recording }` carries the scripted conversion's readable
    recording. It is ingested, profile-checked and compared with the shown document at
    once, off the reader's path.
  - *"Meaningfully different"* is what a reader would see (`difference`): words as a
    multiset (moved text is not new text) and image sources. Fewer than 8 changed words,
    or under 1% of the document's words, with the same images, is not meaningful. Then the
    event is `UpgradeKept`, and nothing on screen changes. Otherwise the document is held
    and the event is `UpgradeReady`.
  - *How "replaced" appears.* The UI sends `Swap { session, anchor }` at an idle moment
    of its choosing: no scroll in progress, no input, nothing animating. The anchor is the
    element at the top of the viewport and the offset into it. The answer is
    `Replaced { anchor }`, the same reading position in the new document. The UI keeps the
    old scene on screen until the new one is composed, scrolled to that anchor, and
    presents it in one frame. There is no blank frame, no fade and no jump. A page whose
    scripts inserted content above the reader stays still under them, because the
    position is an element and not a scroll offset.
  - *The anchor* (`map_anchor`). The element is found in the new document by identity:
    its `id`; otherwise its tag, its own text and the attributes that name content.
    Among elements that share a key, it is matched by ordinal, and only when both
    documents have the same number of them. If the element did not survive, the nearest
    one that did is used, searching document order above first, then ancestry, and the
    reader lands at its top. If nothing near survived, the reader goes to the top
    (`How::Top`).
  - *Refused.* A swap while the reader has taken a step is refused: that state would be
    lost, so the swap waits until they are back at the start. Also refused: an upgrade
    for a different URL, a second upgrade, and a table recorded against a document the
    session never held.
  - *What a session carries* (the link and image bounds) follows what it shows. It is
    built from bytes the broker was handed. The store keeps the shown recording with its
    table composed in, so a session saved after its table arrived restores with it.
  - Both hosts implement this: in process, and jailed over the worker wire (`UPGR`,
    `SWAP`, `ATCH`). The jailed broker keeps its own copy of what it staged, and refuses
    a worker answer that its copy does not allow.

*Step 3, exploration off the critical path.*
- The engine calls back when the scripts have settled (`ScriptEngine::run_settled`).
  That is after load, lifecycle events, timers and the reader's sweep, and before
  exploration. The readable artifact is taken there (`artifact::emit_readable`): the
  recording without a transition table, carrying the measurements a control conversion
  reports.
- Exploration's output follows as its own artifact (`atrium-navigator-transitions/1`,
  `emit_transitions`). It is bound to the readable recording by the BLAKE3 of its bytes.
- `artifact::merge` splices the two back into the recording `emit` would write, and
  refuses a table recorded against another document. The driver still writes the whole
  recording to `PRERENDER_EMIT_DIR` (the merge; a merge that fails fails the document)
  and keeps the parts in `PRERENDER_PARTS_DIR`.
- The broker attaches the table when it arrives (`Attach`):
  - on the shown document at once (`TransitionsAttached`);
  - on the held one at its swap (`TransitionsHeld`);
  - if it belongs to a scripted document kept out as not different, that document
    becomes the held one (`UpgradeReady`). A table may be replayed only on the document
    it was recorded on, and by definition that swap shows nothing new.

*Step 4, fetches in parallel* (navigator-fetch, which fetchd runs).
- *HTTP/1.1 keep-alive.* A connection carries another request only when the body was
  delimited (a length, or chunked with its trailers read), nothing past it was read, the
  server did not say `close`, and the protocol is 1.1. Up to 6 idle connections are kept
  per origin. A GET is retried once on a fresh connection if a reused one fails before
  any response byte.
- *`Conversation::sibling`.* This gives another thread the same conversation: one pin
  table, locked across resolve-and-pin (threat model §2.1), and one idle pool.
- *`prefetch::Prefetcher`.*
  - Hinted URLs are fetched on a bounded pool, 6 workers by default, and each result is
    handed over once.
  - A request never waits behind the queue: a queued hint is taken out and fetched on
    the asking thread.
  - It is bounded by count (512) and by bytes: the relay tells fetchd its remaining
    budget (`B`), and past it queued hints are dropped.
- *The converter's side.* After the parse, the converter hints every stylesheet, image
  and script the parsed document names, in the order it will need them. The relay passes
  the hints to fetchd and never answers them.
- *fetchd.* fetchd's loop is now `navigator_fetch::serve`, portable and tested on
  macOS. Off FreeBSD, `--unconfined-for-testing` serves without a sandbox, with the same
  loop, protocol, address rule and pins, so the real fetch path can be measured on a
  developer machine.

*Found while measuring, and fixed: the normalizer's quadratic.* The alignment pass
collected each element's state slots by scanning every slot, for every element. The GNU
coreutils manual (2.1 MB, 48,322 elements) spent 27 of its 27 s of normalize there. The
pass now takes a range of the handle-ordered map: the same keys, in the same order. That
page now takes 3.75 s. Normalized output is byte-identical on 206 documents, and the
three sets normalize in 87, 80 and 93 s, down to 49, 44 and 47 s (dd697573).

*Timeouts* (2bf7e838).
- A child that runs out of time now says where it was. Before TIMEOUT, its watchdog
  writes a partial timing line (`at_deadline=js>js.exec`) with each phase's time so
  far.
- The driver keeps the timing line and the parts of a timed-out conversion.
- A progressive conversion that times out after its readable artifact publishes that
  artifact, without transitions; one that times out after only its tier-1 artifact
  publishes that one. The reader already had it.


**Re-measured: time to first display, before and after** (ms; 122 pages, timeouts
counted at the deadline).

| arm | what the reader first sees | p50 | p90 | p99 | under 1 s | from main-document arrival p50 / p90 / p99 | under 1 s |
|---|---|---|---|---|---|---|---|
| cold, before (whole pipeline, serial) | the whole conversion | 19,036 | 94,648 | 182,000 | 2% | 18,526 / 93,790 / 180,000 | 6% |
| cold, tier 1 only (serial) | tier 1 | 8,463 | 45,313 | 105,490 | 3% | 6,830 / 42,211 / 104,521 | 12% |
| cold, step 2 alone (progressive, fetchd serial) | FIRST | 9,044 | 47,113 | 125,114 | 2% | 8,278 / 45,733 / 124,249 | 9% |
| **cold, steps 2 + 4 (progressive, fetchd parallel + reuse)** | **FIRST** | **3,295** | **11,355** | **19,902** | 2% | **2,471 / 6,992 / 15,813** | **14%** |
| · the same, its scripted document | readable | 5,435 | 20,464 | 47,543 | 2% | 4,510 / 16,643 / 46,301 | 7% |
| warm, before (whole pipeline) | the whole conversion | 2,553 | 11,174 | 29,008 | 16% | 1,198 / 8,471 / 26,442 | 43% |
| warm, tier 1 only | tier 1 | 1,609 | 6,974 | 16,074 | 30% | 400 / 3,418 / 13,987 | 71% |
| **warm, after (progressive)** | **FIRST** | **1,421** | **4,408** | **10,946** | 33% | **376 / 2,691 / 4,945** | **74%** |

- *Cold.* From the main document's arrival, p90 is 93.8 s → 7.0 s (13×) and p99 is
  180 s → 15.8 s. Every page now gets a first display before the deadline: corriere at
  28 s, the slowest. The child waited 2,877 s in total for fetches with fetchd serial,
  and 606 s with fetchd parallel. Step 2 alone barely moves the cold distribution
  (p90 45.7 s): its first artifact still waits for the parsed document's images, one at
  a time. Step 4 is what moves it.
- *Warm.* From the main document's arrival, p90 is 8.5 s → 2.7 s, and under-1 s goes
  from 43% to 74%. The first artifact arrives 8–70 ms after the main document; what
  remains is display.
- *The goal is not met.* Warm, 74% (not 90%) are under 1 s from arrival. Cold, it is
  14%: 75% of the post-arrival time to a first display is the tier-1 artifact waiting
  for its stylesheets and images, and normalize is 21%.

**Kept: reproducibility and the exploring-vs-control identity** (offline, all 122;
batch 2 from the clean re-run).
- Two progressive runs are byte-identical in the whole recording and in all three parts:
  **122/122**.
- The readable part equals the recording of a control run (exploration off,
  non-progressive): **122/122**.
- The merge of the parts equals the step-1 binary's whole recording: **122/122**.

**Then built, the next cut: an index of rules by subject compound** (1ee0c748). The
normalizer tried every rule on every element. Elements are now indexed by id, class
token and tag, in document order, and a selector is tried only on the bucket its subject
compound requires: its id, else its rarest class, else its tag. Each bucket is a superset
of the matches, in the same order, so the output cannot change, and it does not:
- normalized output and reports are identical on **320 documents** (the 85-recording
  corpus, and both batches' scripted and tier-1 recordings);
- match steps went from 2.94 B to 0.54 B, and per document from p50 3.1 M / p90 28.5 M
  to 0.32 M / 4.6 M. spiegel.de went from 48.7 M to 2.4 M.
- Wall time was measured interleaved, 3 runs each, while the other workload ran, so it
  is given as ratios: spiegel.de 1.80×, ko.wikipedia 1.69×, the coreutils manual 1.05×.
- Matching is no longer most of normalize. The rest (declaration expansion,
  allocation) is the next profile.
- The warm "after" row above predates this index, so the warm first display is better
  than the table says by an amount not yet measured on a quiet machine.

**Then built, cut 1: a first display that does not wait for images** (2026-11-05).
- *Converter* (`subresource::collect_first`, `Stage::Images`). The first artifact
  fetches the document's stylesheets and no image. Each `<img>` is written `pending`:
  - `declared`: the page gives both `width` and `height`, and the box is that size;
  - `placeholder`: it does not. No natural size is claimed (`null`, ratio `none`), so
    the box is what the page's CSS makes it, else the default object size, 300 × 150.
    The image is not dropped, as the normalizer drops an image it has no entry for,
    because then its arrival would insert a box.
- *The images artifact* (`atrium-navigator-images/1`, `IMAGES` frames). The images are
  then fetched (fetchd already has them hinted), before any script is fetched, so the
  scripted conversion finds every one in the memo and is no later than before.
  - Each frame is bound to the first artifact by the BLAKE3 of its bytes and carries
    the whole images block so far. Partial frames go at most every 250 ms, at most 32,
    then one complete frame. A page with nothing pending sends none.
  - `artifact::merge_images` splices the block into the first artifact. For the
    complete frame that is, byte for byte, the first artifact a conversion that waited
    for its images writes. The driver writes it (`<stem>.first-images.json`), and fails
    a conversion that ended with an image pending and no complete frame.
- *Broker* (`Images`, `ImagesAttached { arrived, resized, complete }`;
  `progressive::FirstImages`; backend spec "Images after the first display").
  - The session's recording becomes the merged one, so what it carries names the
    arrived addresses, and only then may their pixels be asked for. Decoding is
    unchanged: the per-document worker, the painted size, the per-document budget.
  - `resized` lists the boxes that may change. Empty, the pixels fill boxes already
    laid out. Otherwise the UI lays out again at an idle moment at the reader's anchor
    (the element mechanism of step 2: on an unchanged document, the same element at
    the same offset), in one frame.
  - Refused: another document's images, any after the complete one or after a swap,
    a "complete" frame with an image still pending, and a malformed entry.
- *Layout shift, measured* (`examples/layout_shift.rs`: the first artifact and the
  merged one, normalized and rendered at 800 × 600, text runs matched in order, a run
  moved if its origin moved more than half a pixel; warm run, all 92 pages with an
  image pending). 3,468 images were pending: 2,055 declared and 1,413 placeholders.
  2,353 arrived at a sizing different from the one shown, or failed.

  | pages | any text moved | text in the first screen moved | worst |
  |---|---|---|---|
  | 19 whose pending images were all declared | 1 (edX, below the first screen) | **0** | — |
  | 73 with a placeholder | 50 | 18 | bbcgoodfood: 12 of 33 first-screen runs, up to 511 px |

  - Declared sizes hold. A declared box changes only where the page's CSS takes the
    height from the image's own ratio (`height: auto`) and the real ratio differs;
    that happened once, below the fold.
  - The text-run shift score (moved runs' area in the first screen, before ∪ after,
    times the largest move as a share of its height) is at most 0.050, and 0.045 or
    less on every other page. It counts text, not element boxes, so it understates CLS
    as a browser computes it; read it as a ranking.
  - The 18 first-screen shifts happen once, when the images arrive, while the reader
    is at the top: cold, a median 0.6–1.8 s after the first display (per run), and at
    p90 5.5–6.2 s.
- *Kept* (warm, offline, all 122, against the 05e8d960 binary): whole recordings
  identical **122/122**; first plus complete images identical to the first artifact
  that waited **122/122** (the raw first artifact differs on exactly the 92 with an
  image pending); readable and transitions parts identical 122/122; two runs identical
  in every part, 122/122 (images artifacts 92/92); the same blob sets. Suites:
  converter 414, backend 226, renderer 152, normalizer 72, fetch 32 tests, all passing.

**Re-measured, cut 1** (ms; the same 122 pages, method and machine). The network
differs from the morning's, so each new arm ran beside a base arm (the 05e8d960
binary). The order was base, new, new, base, batch by batch, and both pairs are given.
None timed out, and fetch failures were in the usual range (54–415 per arm).

| arm | what the reader first sees | p50 | p90 | p99 | under 1 s | from main-document arrival p50 / p90 / p99 | under 1 s |
|---|---|---|---|---|---|---|---|
| cold, base (steps 2 + 4), pair 1 | FIRST, with images | 3,683 | 10,420 | 21,791 | 2% | 2,702 / 7,395 / 20,265 | 11% |
| **cold, cut 1, pair 1** | **FIRST, images pending** | **2,581** | **7,054** | **13,289** | 7% | **1,638 / 4,715 / 7,792** | **23%** |
| · the same, all its images in | | 3,602 | 10,612 | 22,656 | 4% | 2,501 / 7,070 / 15,059 | 15% |
| **cold, cut 1, pair 2** | **FIRST, images pending** | **2,668** | **6,215** | **12,530** | 7% | **1,582 / 3,956 / 7,151** | **26%** |
| cold, base, pair 2 | FIRST, with images | 3,643 | 10,635 | 23,698 | 3% | 2,546 / 6,681 / 22,563 | 19% |
| cold, scripted document (readable), base / cut 1 | readable | | | | | 4,590 / 17,282 / 48,133 ; 4,492 / 17,229 / 39,941 | |
| warm, base | FIRST | 1,325 | 3,833 | 10,636 | 36% | 256 / 2,100 / 4,131 | 80% |
| warm, cut 1 | FIRST | 1,295 | 3,829 | 10,607 | 37% | 258 / 2,149 / 4,115 | 80% |

- *Cold.* From the main document's arrival, p90 falls 7.4 s → 4.7 s (pair 1) and
  6.7 s → 4.0 s (pair 2), and p99 falls about 20 s → 7–8 s. Under 1 s goes 11% → 23% and
  19% → 26%. corriere's first display is no longer the slowest: the cold maximum went
  from 25 s to 13–15 s (spiegel.de, 11.5 s from arrival, is now the slowest). The scripted document is not later (p90 17.2 vs 17.3 s); it
  waited for the images before, and still does.
- *All images in* lands where the first display used to be (p90 7.1 vs 7.4 s). Images
  now arrive into a page the reader already has.
- *Warm* is unchanged, as it should be: offline, images cost milliseconds. Warm under
  1 s from arrival is now 80% (74% this morning). That is the rule index (1ee0c748),
  which the morning's warm row predates, measured on a quiet machine.
- *The goal is still not met.* Cold, 23–26% of pages show something within 1 s of
  their main document. What is left is the stylesheets, which the first display does
  wait for, and normalize.
- Data: `~/src/navigator-corpus/ttfd-images-2026-11-05/` (`final.sh` reproduces the
  table, `check.py` the identities, `shift-warm-new.tsv` the layout shift, `bin.sha256`
  the binaries). The first timing run stopped at 21:20 when the session's process
  exited. Its unfinished batch 2 of pair 2 was re-run into fresh directories (`*-2r`),
  and the partial one is not read.

**Then built, cut A: pending images sized from their headers** (2026-11-06, 1e73ca28).
Cut 1's first display gave an `<img>` the page leaves unsized a placeholder box, and the
layout moved when the image arrived: 51 pages, 18 of them in the first screen. Its box is
now sized from the image's own first bytes, fetched while the stylesheets load.
- *The parser* (`navigator_dom::natural::header_size`). It is a strict prefix reader:
  - it answers `Sized` only when `natural_size` of ANY image that starts with those bytes
    gives that size, so the box is the one the whole image gets; otherwise `More(n)` or
    `Unreadable`;
  - it reads at most 64 KiB, in at most four reads (16, 32, 64 KiB);
  - it refuses zero sizes, a side over 16,384 and a frame over the 256 MiB decode budget;
  - it never stores, hashes or keeps the bytes.

  16 KiB answers 2,720 of the corpora's 2,737 images, and 64 KiB all but one (a JPEG
  with 84 KiB of EXIF before its frame header).
- *Fuzzed differentially* (`navigator-dom` `tests/header.rs`). On every prefix of
  mutated seeds of every format, the verdict is checked against `natural_size` of the
  whole image. The test fails unless every outcome class is reached for every format: a
  fuzzer that never reaches `Sized` for a JPEG is not testing JPEG. The 2,739 corpus
  images and 300,000 mutants agree.
- *Fetching* (`navigator-fetch`).
  - The converter hints each unsized image's header (`HH <url>`) after the stylesheets
    and before every image body.
  - fetchd fetches its first 16 KiB with `Range`, reading no more even from a server that
    ignores it, and checks a 206 against `Content-Range`.
  - Jobs run in hint order (sheets, headers, bodies), with two more workers that take
    headers only.
  - The converter peeks (`P <start> <len> <url>`). The relay bounds each peek and charges
    it to the per-conversion budget.
  - *The header window* (250 ms from the first peek) is fetchd's, beside the network's
    other timeouts, so no converter decision reads a clock (threat model §2.6). To the
    converter a late header is a fetch that failed.
- *Pending says how the box is sized.* `header` is the new word. An image whose header
  **proves** it will fail is left out, as the final recording leaves it: not an image at
  all (a `src=""` that returns HTML), an HTTP error, a refusal, or offline. A late header
  stays a `placeholder`. The broker accepts `header`; such an image arrives with
  `resized` empty.

**Then built, cut B: what the first display waits for, profiled and cut** (2026-11-06,
e23e712d). Time from the main document's arrival to the first display was split per page
(timing checkpoint at the first artifact; normalize by phase, `navigator_normalize::
phases`; `examples/display_profile`). On cut 1, summed over 122 pages:
- cold: the stylesheet wait is 66%, normalize 27%, render 6%, and parse 0.3%;
- warm: normalize is 80% and render 18%.

A preload scan before the full parse could save at most the parse (p50 3 ms), so it was
not built. Fixed, with every output byte-identical:
- *Imports early.* Every top-level sheet is taken as it arrives and its `@import`s are
  hinted at once (13 of 122 pages import), then theirs. The unchanged walk then finds them
  fetched or in flight. Also fixed: an `@import` with no `;` was cut at byte 256 even
  inside a multi-byte character, which panicked the converter.
- *Margin collapsing.* Everything a chain walk asks of an element is remembered per media
  context. `empty(h)` walked h's whole subtree and was asked again at every level above
  it. It was 31% of normalize on first displays, quadratic on deep pages, and is now 4.2×
  faster on the five slowest.
- *The renderer's cascade* (navigator-style). It matched every rule against every
  element. A normalized document has one class per distinct block, so that was rules ×
  elements. Rules are now indexed by their subject compound's id, class or tag and tried
  in their own order. Each longhand's grammar row is looked up once, not per element per
  property.
- *The normalizer's cascade.* A declaration with no `var()` is expanded once per rule,
  and its report entries are replayed on each use.
- *Kept.* The normalized document, the report and the rendered scene are identical on
  **421 documents**: the first, first-with-images and readable artifacts of all 122 pages,
  and the 85-recording corpus, against 0beab757 (`examples/display_digest`).

**Re-measured, cuts A + B** (ms; the same 122 pages, method and machine; base = 0beab757,
cut 1; new = e23e712d; ABBA per batch: base, new, new, base, cold then warm; none timed
out).

| arm | p50 | p90 | p99 | from main-document arrival p50 / p90 / p99 | under 1 s from arrival |
|---|---|---|---|---|---|
| cold, base, pair 1 | 2,373 | 6,508 | 13,793 | 1,395 / 3,901 / 6,806 | 30% |
| **cold, new, pair 1** | **2,236** | **5,704** | 20,257 | **1,353 / 3,241** / 16,395 | 33% |
| **cold, new, pair 2** | **2,459** | **5,996** | 13,949 | **1,603 / 3,759** / 7,248 | 27% |
| cold, base, pair 2 | 2,492 | 5,776 | 13,164 | 1,392 / 3,687 / 7,843 | 31% |
| warm, base (×2) | 1,292 / 1,291 | 3,808 / 3,790 | 10,614 / 10,595 | 253 / 2,081 / 4,013 | 80% |
| **warm, new (×2)** | **1,208 / 1,202** | **3,176 / 3,162** | **10,324 / 10,327** | **126 / 1,099 / 2,081** | **88%** |

- *Warm* is the display path. From arrival, p90 is 2.08 → 1.10 s and p99 4.0 → 2.1 s.
  Under 1 s rises from 80% to 88%; both pairs agree within 10 ms. Summed over the 122
  first displays, normalize is 63.0 → 34.7 s and render 14.4 → 5.8 s. What remains of
  normalize: cascade matching 34% of it, collapse 14%, table pre-measure 10%, custom
  properties 8%.
- *Cold* is the network.
  - The stylesheet wait is now 79% of the time from arrival, normalize 14% and render 2.5%.
  - Header peeks add at most the 250 ms window (p90 255 ms), on the pages that have late
    headers.
  - Pair 1's p90 improved (3.90 → 3.24 s); pair 2's did not (3.69 → 3.76 s).
- *One page regressed in both new arms*: postgres (`335_postgres_select`).
  - Two of its stylesheet fetches failed after about 34 s each, and its first display came
    at 65–68 s, against 13 s on base. That is the new arms' p99 and maximum.
  - Run alone afterwards with the same binaries, it was clean three times out of three:
    all five sheets, first display at 2.1–8.0 s.
  - **Cause not established.** The likeliest is that header jobs add connections to one
    origin, tripping a server-side limit during that window. Open.
- *Header sizing, cold* (`c.header.*`). Of 1,426 unsized images:
  - 41% were sized from their header and 57% were late (the window closed first);
  - 2% were left out as proven failures.

  Late headers are concentrated on image-heavy pages (ikea ~70 unsized images,
  ocw.mit.edu ~80, spotify ~83). Those pages' lower images still move when they arrive.
- *Layout shift* (`examples/layout_shift.rs`, first artifact vs first + images, 800 × 600):

  | arm | pages with any text moved | pages with first-screen text moved | placeholders |
  |---|---|---|---|
  | warm, cut 1 | 51 | 18 | 1,413 |
  | **warm, cut A** | **13** | **0** | 3 |
  | cold, cut 1 | 51 | 18 | 1,413 |
  | **cold, cut A** | **31** | **1** (thomann, 34 late headers) | 812 |

  Warm, every first-screen shift is gone. Before cut A's last change, the two warm
  survivors were an `<img src="">` (samsung) and an SVG whose fetch fails (vanguard); both
  are now left out, as the final recording leaves them. Cold, the late headers fall mostly
  below the first screen, because headers are asked for in document order.
- *Kept* (warm, all 122, against 0beab757):
  - first display with its images, base == new: **122/122**;
  - each first artifact differs from base's only in its images block: 122/122;
  - whole recordings, readable and transitions parts identical: 122/122;
  - two new runs identical in every part: 122/122 (images artifacts 88/88);
  - the same blob sets.

  Suites: converter 419, backend 228, renderer 152, normalizer 72, fetch 40, dom 17 and
  style 35 tests, all passing.
- Data: `~/src/navigator-corpus/ttfd-headers-2026-11-05/`:
  - `ttfd.py` reproduces the table, `breakdown.py` the split by phase and `headers.py` the
    header outcomes;
  - `check.py` and `check2.py` the identities, `shift-*.tsv` the layout shift;
  - `bin.sha256` the binaries.

  Three isolated postgres reruns overlapped the first warm new arm of batch 1 (01:13–01:15
  IST). They are network-bound and small, and both warm new runs agree within 10 ms.

**Open, and the next cuts, in order of what they would move** (*superseded 2026-11-05:
the 1 s goal is dropped and these are parked; see the goal above*).
1. **Stylesheets.** Cold, they are now what a first display waits for. Next: fetch a
   page's sheets ahead of its body's other subresources (they already lead the hints),
   HTTP/2 to their origins, and a stylesheet cache across conversions. The
   placeholder shift (18 pages in the first screen) is the price of not waiting; a
   page that wants none declares its images' sizes.
2. **Normalize**, the warm remainder: profile after the index, then the
   declaration-expansion path.
3. **More concurrency per origin than six HTTP/1.1 connections** (HTTP/2), and DNS
   prefetch for the hinted hosts.
4. JS: a bytecode cache for the common libraries (the per-library tail above). Layout:
   incremental relayout for ynet-like pages, whose 8 layouts take 22 s.
5. The FreeBSD fetchd main is not compile-checked on this machine (there is no FreeBSD
   sysroot here). Its only new code is the `Locked` wrapper and the call into `serve()`.
6. The datacenter view: the same arms from a cloud VM, to separate the network from the
   pipeline. Deferred: the user chose to keep measuring locally for now.

### 5.5 WebAssembly is a better cage, not a translation

A recurring hope is that WASM offers a way out. It does not, for a simple factual reason:

- **Pages do not ship a WASM equivalent.** WASM is used by a small minority of sites, for
  compute-heavy work (design tools, games, codecs, emulators). There is no dual-publishing
  convention and no site that offers "the WASM version" of its interactivity.
- **WASM cannot drive a page by itself.** It has no direct DOM access; it interacts through
  imported JavaScript functions. So a WASM-using page runs JS *as well*, never instead.

Compiling JS to WASM ahead of time does not rescue the transcriber either. Such tools
either embed a JS interpreter in the module — leaving the semantics exactly as they were —
or support a subset, which runs into §5.2's limits unchanged. Either way arbitrary code
still executes at view time, which is the thing §5.1 was protecting.

WASM does have two legitimate roles here, and it is worth keeping them distinct:

1. **The secondary lane, already settled** (atrium-navigator.md §3.2): any language →
   WASM-IR → AOT (Cranelift) → native jailed app. This is a portability on-ramp for *new*
   apps whose output is a primary-model native binary. The content-addressed economics
   apply cleanly — a module is AOT-compiled once per hash and the result deduplicates.
2. **Defence in depth inside the legacy jail** (optional): running the engine compiled to
   WASM adds memory safety over the engine's own bugs, at a real performance cost. It is
   complementary to the jail rather than a replacement — the jail bounds OS-level escape,
   WASM bounds memory-level corruption — and it does **not** address view-time side
   channels, because the code still runs on the reader's machine.

The summary worth remembering: WASM changes the *sandbox*, not the *source material*. It
belongs in the same category as the jail, and it does not move anything into the
declarative lane.

### 5.6 Converter integrity

Two risks that arrive with any shared conversion pipeline, both flagged rather than solved:

- **Cloaking.** A site can serve different content to a converter than to a browser.
  Since the conversion is what readers see, the converter is a targetable surface.
- **Shared-artifact poisoning.** A conversion is served to every subsequent reader of
  those source bytes, so a bad conversion has broad blast radius. Conversions should
  record their exact source bytes and environment, and be independently reproducible, so
  a disputed artifact can be re-derived and compared rather than trusted.

## 6. Output is a scene graph, not pixels

**Decision: the legacy engine emits a Fresco scene graph** (Servo display list → scene
graph), not a pixel stream.

Pixels are much cheaper to port and are what isolation products ship, but they discard
everything that makes content usable: selectable and searchable text, accessibility
semantics, resolution independence, theme and text-size adaptation, and cheap
re-composition on scroll. A pixel lane would also make the legacy experience visibly
second-class in a way that pushes users back to a real browser.

Consequences that follow and must be built, not assumed:

- The scene graph from a legacy worker is **untrusted input**, exactly as a document
  worker's is, and passes the same validator with the same hard limits (backend spec §4.2).
  It is a richer structure than a pixel buffer, so the validator matters *more* here.
- Semantics must be carried across the port (the engine's accessibility tree → our
  semantic tree), or the legacy lane loses G5 and we have two different accessibility
  stories.
- A pixel fallback is acceptable as a *porting milestone*, never as the shipping design.

## 7. Isolation and sharing — where the line actually goes

The natural cost objection is that a whole engine per site is heavy. The natural response
is to split the engine: something dangerous per-site, everything else shared. That
instinct is right, but the line is not "security part vs the rest", because almost
everything downstream of parsing is site-tainted: layout operates on the site's DOM,
painting operates on the site's display list, decoding operates on the site's bytes. The
non-tainted remainder is smaller than it looks.

### 7.1 The rule

> **Share immutable data. Never share a live service that touches site-private input.**

Sharing read-only *data* (code pages, the shipped font set) has no channel: nothing
flows back. Sharing a *service* creates one in both directions — the service sees every
client's input, and a bug in it is reachable by every client.

### 7.2 What is safe to share

- **The engine's code pages.** This is ordinary shared-library behaviour and is free.
- **The shipped font data**, mapped read-only and shaped **in-process**. Note the
  distinction: a shared font *service* would see which glyphs each site renders, which is
  a text-content side channel. Share the data, not the server.
- **Content-addressed artifacts derived purely from public bytes** — normalizer output,
  for instance — governed by the existing dedup-domain policy rather than a new mechanism.
- **The compositor.** Worth stating plainly: the sharing you want largely exists already
  and is called Fresco. A compositor's entire job is to serve mutually distrusting
  clients, and Limen already composes surfaces across jail boundaries.

### 7.3 What must never be shared, and the hole each would open

- **A shared image/media decoder.** Decoders are historically the richest source of
  remote-code-execution bugs in any browser. A decoder shared between sites is a confused
  deputy: compromise it from site A and read site B's decoded content. Decoding belongs
  inside the per-site jail.
- **A shared JIT or JS runtime.** JIT bugs yield arbitrary read/write and are the most
  attacked component in any engine. A shared JIT service would be the single highest-value
  target in the system, reachable by every site at once.
- **A shared code or resource cache across sites.** This is an existence oracle: site A
  detects whether site B loaded resource X by timing. We have already settled this
  analysis for Tessera (tessera-fs.md §20) and the same conclusions apply — but with a
  crucial difference, see §7.4.
- **Any single address space holding two sites' data.** Microarchitectural leakage
  (Spectre-class) makes software boundaries inside one address space unreliable. The
  industry spent a decade trying the software way and then moved to process-per-site; we
  should not re-derive that result experimentally.

### 7.4 The legacy lane loses the document lane's structural protections

This is the point most easily missed. Much of the document lane's safety comes from the
JS-free invariant: no scripted timer and no scripted fetch means the classic cache-probing
and fingerprinting primitives have no vehicle (profile §1.1). **In the legacy lane,
attacker-controlled code runs.** Every one of those mitigations is gone, and timing
oracles, fingerprinting and cache probing are live again.

So the legacy lane must be reasoned about as if it were a browser, because for these
purposes it is one. It inherits none of the document lane's guarantees, which is precisely
why the lane must be visible to the user (§8) and why sharing decisions here are stricter
than they would be in the document lane.

### 7.5 What actually solves the cost problem: fork before taint

The real cost of a per-site engine is startup — mapping the binary, initialising the
runtime — not steady-state memory, which read-only page sharing already handles.

**Zygote model:** keep one fully initialised engine process that has never seen any site
content, and fork it per site at navigation. All initialisation work and all read-only
pages are shared through copy-on-write, and cross-site exposure is exactly zero because
the fork happens *before* any site data exists in the process. This is what Chrome and
Android do, and it gets the user's sharing benefit with none of §7.3's holes.

It composes well here: Tessera's CAS already shares the engine's on-disk image, and jails
are the unit we fork into.

### 7.6 The isolation unit is the origin

Browsers isolate by site (eTLD+1) rather than origin, a compatibility concession to
existing content. **We have no such debt, so the unit is the origin** — strictly stronger.
Cross-origin embedding, the out-of-process-iframe problem that took browsers years to
retrofit, is native here: Limen composes separate jailed surfaces by construction.

With the zygote model the process multiplication that origin-granularity implies is
affordable. If measurement says otherwise, the fallback is site-granularity, argued
explicitly and written down — never slid in.

### 7.7 The fetcher needs per-origin credential state

Our current design has one brokered fetcher holding the only network capability (backend
spec §2). It sees every origin's traffic, which makes it a cross-site channel and a
confused deputy if it ever holds several origins' credentials at once. Credential and
connection state must be partitioned per origin, and a fetcher instance should be
ephemeral per request where that is affordable. Flagged here as a gap in the backend spec
rather than left implied.

## 8. Two lanes, and the lane is visible

The user-facing consequence is a two-lane world, and the lane must be legible rather than
hidden: fast, safe and zero-authority by default; compatibility lane with an indicator and
a different trust posture.

Reader mode is the precedent — browsers already ship "strip the cruft and show me the
content", and users like it. This architecture inverts the priority: **reader mode is the
default and the fast path, and the full engine is the fallback.**

### 8.1 Settled shape: a document with jailed islands

*Settled (user, 2026-11-04). The YouTube prototype that tests it is not built yet.*

The two lanes need not be chosen per page. **The default shape of the compatibility lane
is an island:** the page is served by the document lane (tiers 1–3, including §5.4.5's
bindings), and only the region that genuinely needs an engine runs in tier 4, in its own
origin-scoped jail, composed into the document as a separate surface (§7.6: Limen composes
jailed surfaces by construction). A whole page goes to tier 4 only when it is a fat client
throughout (§11.8.3).

**Why islands rather than whole pages:**

- Everything outside the island keeps the document lane's guarantees: no script at view
  time, our semantic tree, our layout, our scene.
- The engine's attack surface and memory shrink to the island's code. YouTube's embed
  player, for example, is a few hundred KB of JS, against the whole single-page app.
- Navigation stays the document's. Following a link is a document navigation, not
  something the island's code controls.

**The island boundary is a typed channel, not shared state.** Where the document must
drive the island, it does so through a narrow, declared message set, and nothing else
crosses. For a video island the set is play, pause, seek and current time, which is what
YouTube's own embed API already exposes over postMessage. The channel is part of the
artifact, so it can be audited. The island never sees the document's DOM, and the
document never runs the island's code.

**Where islands recur** (from §11.8.3): video players, map widgets, payment fields (the
processor's tokenisation iframe), embedded editors (Figma), and DRM playback.

**Nesting: a CDM is an island inside an island.** Where DRM applies (§11.8.3: Spotify,
paid video), the decryption module is the only part that must be a closed binary. Settled
with the user: it runs in **its own jail under Linux ABI emulation**, inside the media
island and below it.

- The CDM is installed by the user, never shipped with Atrium (the licence forbids
  redistribution; this is the route Raspberry Pi OS and Asahi Linux take).
- The CDM jail has **no network**. Licence challenges and responses travel through the
  island's engine, which owns the page network under §5.4.1c's rules.
- The CDM receives only encrypted samples and licence messages, and returns only decrypted
  frames. At Widevine's software level (L3) the frames come back for composition like any
  other video. That is also why services cap resolution there, a cost stated rather than
  hidden.
- It is the most strongly jailed component in the system, because it is the one we can
  neither read nor build. Firefox runs its CDM in a dedicated sandbox for the same reason.

**What must be handled, not assumed:**

- *Embedding refused.* Some content forbids embedding (YouTube's per-video setting,
  age-restricted videos that need a session). The fallback is the whole page in tier 4,
  and it is visible as such (§8).
- *Mutating actions around the island* (like, subscribe, comment) are reader-issued request
  templates (§5.4.5.2) in the document, or go through the island's own session with that
  origin's partitioned credentials (§7.7). Never both for the same action.
- *Accessibility.* An island is opaque to the semantic tree until open question 2 is
  answered. The island's region is labelled as such, so a reader knows what is unreachable.
- *Lane visibility* (§8) applies per region: an island carries the compatibility lane's
  indicator, not the whole page.

**First prototype: a YouTube watch page.** The document lane serves the title, description,
chapters, comments and recommendations. A tier-4 island runs `youtube.com/embed/<id>`, with
MSE in the engine and royalty-free decoders (dav1d for AV1, libvpx for VP9; OpenH264 for
H.264, as Firefox does). Chapter links seek through the typed channel. No CDM is involved:
ordinary YouTube videos are not DRM-encrypted.

## 9. Honest costs

- A full engine port (Servo → Fresco scene graph, jailed, brokered network) is the largest
  single piece of work in D6, which is why it is last in the build order.
- A local engine per origin is heavy — though no heavier than what a browser already does
  on the same page, and it is not the default path.
- Prerendered artifacts go stale.
- The legacy lane offers none of the document lane's six guarantees, and saying otherwise
  would be the most damaging overclaim available in this design.

## 10. The jail monitor — the jail refuses, the monitor reports

**Decision: ship a jailed JS engine (§2), and pair every jail with a monitor that reports
attempts to reach what the jail forbids.** Confinement and detection are complementary:
the jail is why the attempt fails, the monitor is how anyone learns it happened.

This generalises beyond the browser to every Portcullis jail, and if adopted
platform-wide it should graduate into its own spec rather than living here.

### 10.1 Why the signal-to-noise is exceptional here

Host intrusion detection usually drowns in false positives, because normal software
legitimately does a wide variety of things and the baseline is noisy. That objection does
not apply to these workers, and the reason is worth stating precisely:

> **A zero-capability document worker has a near-empty legitimate syscall profile** —
> read a pipe, write a pipe, map anonymous memory, exit. Anything outside it is a bug or
> a compromise.

There is no third explanation to sift for. The alert budget is tiny and every alert means
something, which is the opposite of the usual IDS economics. **The jail is what makes the
detector good**: by emptying the baseline, confinement converts detection from a
statistical problem into a binary one.

This also sets the implementation style: **deterministic rules, not a learned model.**
A rule like "this worker attempted `open()`" is reasonable about, testable in CI, and
cannot drift. An anomaly model is none of those.

### 10.2 The refusal path reports; nothing infers

The monitor should not be an external observer inferring intent from the outside. The
**denial points themselves emit structured events** — the jail check, the capability
check, the broker's rejection — because a refusal that is not reported reads exactly like
a refusal that never happened. This project has been bitten repeatedly by silent-drop
paths, and an unreported `EPERM` is one.

### 10.3 Two rules this project has already paid for

1. **On by default, never a debug switch.** An audit you enable by hand is off at the
   moment it matters — established while chasing snapshot damage, where every run that
   produced damage had the audits off because nothing turned them on.
2. **The monitor must be provably able to fire.** It needs a fault-injection path that
   makes a worker attempt a forbidden access, with a test asserting the monitor reported
   it. Shipped with the monitor, not added later. A detector that cannot fire reports
   zero and reads exactly like a clean system — which is not hypothetical: a snapshot
   detector in this tree returned "clean" across 122,707 checks while being structurally
   incapable of failing.

### 10.4 What it catches — and what it does not

**Caught:** a compromised worker probing for escape. Opening files, opening sockets,
enumerating processes, touching devices, attempting `execve` — the reconnaissance that
follows almost every exploit, and the first thing an attacker does after landing.

**Not caught, and this must not be overclaimed:**

- **Compromise that stays in-lane.** An exploit achieving its goal entirely through
  *allowed* channels never trips a denial. The monitor detects escape *attempts*, not
  compromise.
- **Side channels.** Spectre-class leakage produces no syscall and no denial.

So the honest claim is "we detect attempts to leave the box", not "we detect intrusion".

### 10.5 The allowed channels are the real exfiltration risk

Because §10.4 leaves in-lane compromise invisible, the allowed channels deserve the
scrutiny the forbidden ones get automatically:

- **The scene-graph pipe.** A compromised worker can encode data into a structurally
  valid scene graph. The validator (backend spec §4.2) bounds its *size and shape*, not
  its *content*, and no practical mechanism closes a steganographic channel here.
- **The brokered fetcher.** Network access is the one genuinely useful exfiltration path,
  which is why the per-origin partitioning of §7.7 is load-bearing rather than tidy: a
  worker confined to fetching from its own origin can leak the site's data *to that
  site*, which already had it. **A fetch request outside the worker's origin is exactly
  the alert worth raising**, and it is a rule the monitor can state exactly.

### 10.6 The monitor is itself a shared service

§7.1's rule applies to the monitor: it observes every jail, so it is a cross-site
aggregation point and a confused-deputy candidate. Consequences:

- It must not be reachable *from* the jails it watches.
- Its input is attacker-influenced, so its own parsing must be minimal and its store
  append-only.
- **Its contents are browsing history.** Denial records name origins and times, which is
  among the most sensitive data on the machine. Reports stay local by default; nothing
  leaves the machine without an explicit, per-destination decision by the user.

### 10.7 Response policy: report always, kill carefully

Killing a jail on its first denial is tempting and wrong as a default — real libraries
probe for `/dev/urandom`, locale files and `/proc` during start-up, so first-denial
termination converts a benign probe into a denial-of-service, and a site could trigger it
deliberately. The staged policy:

1. **Always report.** No denial is silent.
2. **Baseline, then enforce.** Learn each worker class's legitimate profile with the
   monitor in report-only mode, then promote stable rules to terminating ones.
3. **Terminate on exploitation patterns**, not on single probes — repeated distinct
   denials, denials after a parse of hostile input, attempts at `execve` or device nodes.

Because the baseline is near-empty (§10.1), this staging converges quickly rather than
becoming a permanent tuning exercise.

### 10.8 A user-facing signal worth having

The empty baseline makes an unusually concrete statement possible: *this site's renderer
tried to read your files*. That is a far better security signal than the padlock the web
offers, precisely because it reports an observed event rather than a property of the
transport. The discipline is not to spend it — surfacing routine probes as alarms trains
users to dismiss the one that matters.

**Naming:** the Roman night watch were the *vigiles*, which fits both the convention and
the job; `vigild` is offered as a candidate, not a decision.

## 11. Engine selection and execution mode

### 11.1 Two decisions, not one

"Servo or build our own" merges two choices. **Servo is a browser engine, not a JS
engine** — it brings SpiderMonkey with it. And the JS engine is not independently
selectable in practice, because the hard part is not the VM, it is binding the VM to a
DOM. Choose the browser engine; the JS engine arrives attached.

### 11.2 We do not write an engine *now*

**Decision: adopt a mature engine for the legacy lane today. This is a sequencing
decision, not a permanent verdict** — see §11.8, which keeps a memory-safe engine open as
the architecturally correct end state.

Reasons it is not the thing to build first:

- ECMAScript conformance is measured against test262's tens of thousands of tests. A
  credible engine is many engineer-years of the most security-sensitive code in the
  project, and it is not currently the bottleneck — tiers 1–3 are (§5.3).
- Nothing about the engine choice is decidable before §5.4's corpus measurement. Building
  an engine to serve a lane whose size is unmeasured is the same error as porting Servo
  before measuring (§11.4).
- A fresh engine is less *compatible* than a mature one for years, and in the legacy lane
  compatibility is the entire purpose — an incompatible legacy engine has no reason to
  exist.

Note what is **not** among the reasons: that it is hard or large. This project builds
filesystems, schedulers and compositors on the argument that doing it right matters more,
and "too much effort" would be inconsistent with that charter. The argument here is about
*order*, and about the legacy lane specifically being a fallback rather than the main
path.

### 11.3 The shortlist, and what the licensing policy decides

| candidate | licence | verdict |
|---|---|---|
| **WebKit / JavaScriptCore** | LGPL | **Hard reject.** LICENSING-POLICY.md admits no LGPL anywhere in the runtime stack |
| **Chromium / Blink / V8** | BSD-3 | Licence fine; rejected on architecture — colossal, own build system, and it assumes its own process and sandbox model, which is precisely what we are replacing |
| **Gecko** | MPL-2.0 | Enormous, never designed for embedding |
| **Servo** (+ SpiderMonkey) | MPL-2.0 | **Selected.** Rust, embeddable by design, actively maintained. MPL-2.0 sits in the policy's "evaluate per-component" bucket, and toolkit-backends.md already set that precedent |

Servo was already position 5 of the D6 build order; this confirms it rather than changing
it. Its weaker web-compatibility relative to Blink is tolerable *because* tiers 1–3 carry
the reading cases (§5.3) — it is a fallback, not the main path.

### 11.4 Sequencing: measure before porting the expensive thing

**Do not start with Servo.** Tier 2 — prerendering SPA content in the normalizer — does
not obviously need a browser engine. **A JS engine plus a minimal DOM** is dramatically
cheaper, and it is precisely the instrument that produces §5.4's corpus number — the
measurement that decides whether the Servo port is worth starting at all. Building the
expensive thing first, to discover whether it was needed, is the wrong order.

**Decision: the instrument is Boa plus a minimal DOM.** Unlicense/MIT (allowed outright,
no per-component evaluation), ~95.5% test262, a memory-safe object model, and its one
weakness — speed — is irrelevant to a converter that runs offline.

**Build it with a thin engine boundary.** The minimal DOM is the bulk of the work and is
engine-agnostic; keeping the embedding surface narrow means the same instrument can later
be re-run on a different engine. That is not hypothetical tidiness — it makes the
instrument double as the evaluation harness for §11.8's trigger 1, measuring a candidate
engine against *our* corpus rather than against a published score.

**Honest limit, to be stated with the result:** a DOM without layout fails on content
calling `getBoundingClientRect`, `offsetWidth`, or the observer APIs — the known ceiling
of this approach. The number it yields is therefore a **lower bound** on what tiers 1–3
can cover, and must be reported as one rather than as the answer.

### 11.5 Execution mode: JIT off, AOT to bytecode

**Decision: JIT disabled by default; ahead-of-time compilation to bytecode.**

JIT is the most exploited component of any engine, which is why hardened modes in shipping
products disable it. SpiderMonkey can run interpreter-only; QuickJS has no JIT at all.

**What AOT buys, and what it does not.** It does *not* buy JIT-level performance: JIT's
advantage comes from speculating on observed runtime types, which by definition is not
available ahead of time, so AOT-compiled JS lands around good-interpreter level. What it
buys is the removal of **runtime code generation** — no writable-executable mappings in
the process running hostile code, no JIT spraying, and none of the optimizing-compiler
type-confusion bugs that are the highest-value targets in an engine.

**The realistic form is AOT to bytecode** (the Hermes/QuickJS model), not AOT to native.
It preserves JS semantics exactly, eliminates runtime codegen, starts fast, and produces a
compact content-addressable artifact. Native AOT for JavaScript does not reach JIT
performance anyway, so it pays a large complexity cost for no benefit we need.

**AOT is not a privilege granted to trusted origins.** That framing was considered and
rejected:

1. It reintroduces the trusted fast path that the scene-graph hardening spec eliminated —
   trust-by-identity survives compromise, and the privileged path is what an attacker
   reaches for.
2. Origin trust does not track the risk. A trusted origin is compromised by a CDN
   injection, a supply-chain package or an XSS: **trust attaches to the origin, while the
   risk attaches to the code.**
3. AOT needs no trust. The artifact is produced by *our* compiler in *our* jail, so its
   safety rests on our compiler being sound, not on who authored the source.

**The axis that does make sense is economic: compile by hash popularity, not by origin.**
A framework identified by content hash compiles once and deduplicates across every site
and user — §5.3's tier-3 insight applied to compilation. Per-deploy application bundles
are poor candidates because compiling them is economically pointless, not because they are
untrusted.

**Requirements inherited from elsewhere in this design:**

- The compiler consumes hostile input, so it runs in an **empty jail** (hardening H8.1:
  parse where the capability set is empty), offline and separate from the executing jail.
- Its output is a **content-addressed artifact shared between users**, which is the
  implantation path (hardening H10.1), not the exfiltration one. AOT artifacts therefore
  need the discipline of §5.6: **deterministic compilation**, so any artifact can be
  independently re-derived and compared rather than trusted.

**Honest limit:** this removes the code-generation class, not the engine's attack surface.
GC and builtin bugs are untouched — see §11.6. The jail remains the primary boundary; this
is defence in depth, on the same reasoning as hardening H8.1.

### 11.6 Memory management: don't reclaim, rather than don't collect

**Decision: non-reclaiming allocation in ephemeral workers; conventional GC only where
lifetime demands it.**

The garbage collector is, with the optimizing compiler, one of the two largest sources of
exploitable bugs in any engine. It cannot be removed by changing target language: JS
*semantics* require garbage collection — unbounded object lifetimes, cycles, closures, and
no ownership information anywhere — so any target must implement them (§11.7).

But **GC bugs are a consequence of reclaiming memory.** Where a worker is short-lived and
bounded — a document render, a prerender conversion — it need not reclaim at all:

> Make the worker ephemeral, and give the engine an allocator whose `free` is a no-op.

QuickJS already accepts a custom allocator, so this is configuration, not a compiler
project. What it buys:

- **Use-after-free stops being exploitable.** The primitive is free-then-reallocate with
  attacker-controlled data of a different type; with no reuse, a dangling pointer still
  refers to the intact original object. The bug may remain; the exploitation path does
  not.
- **Collector bugs cease to exist** rather than being mitigated — no mark/sweep races, no
  compaction pointer fixups, no premature collection from a refcount error, because
  nothing is ever collected.
- It introduces **no new code of our own**, which was the decisive objection to §11.7's
  alternatives.

Costs and limits, stated so this is not oversold:

- **Buffer overflows are untouched.** Linear overwrites within and across arena objects
  work exactly as before. This removes one large class, not memory unsafety.
- **Memory grows monotonically**, bounded only by RCTL and by process exit. Fine for a
  document parse; heavier for a large prerender, which is offline and amortised.
- **It does not extend to long-running apps.** A tier-4 application open for hours needs a
  real collector and keeps that surface.
- **Marginal value scales with the capability set** (hardening H8.1, H10.1). In a
  zero-capability worker a use-after-free yields control of a process holding nothing —
  already the assumed state. It is worth materially more in the legacy engine jail, which
  may hold an authenticated session, and which is also where memory pressure is highest.

### 11.7 Rejected: compiling JS to native, WASM, or a custom bytecode

Recorded with reasons, because a rejected option without its reasoning gets re-proposed.

The motivation is sound — removing whole vulnerability classes, GC especially — but
changing the *target* **relocates the attack surface rather than reducing it**. A JS
engine's exploitable surface is mostly its *semantics*, not its execution strategy: the
object model (property lookup, hidden classes, prototype chains, getters, Proxy), the
collector, and the builtins (Array, TypedArray, RegExp — itself a compiler, JSON, String).
Only codegen is about execution strategy, and §11.5 already removes that slice.

Compiling to another target cannot delete semantics; it must implement them, and both ways
of doing so are worse:

1. **The artifact embeds a JS runtime** (the Javy / QuickJS-in-WASM shape) — the entire
   runtime, builtin and collector surface is still present, merely recompiled.
2. **The compiler inlines the semantics** into generated code — prototype-chain and
   coercion bugs now live in *our* compiler's output rather than in an engine that has
   absorbed two decades of adversarial attention. Strictly worse.

Target-specific notes:

- **Native.** Production systems that AOT-compile JS to native require *typed subsets*
  (Static Hermes needs annotations; Porffor supports a subset). Unannotated web JavaScript
  is not that language — the same dynamism wall as §5.2.
- **WASM.** The one target that buys something real: type-checked, control-flow integrity
  by construction, memory-safe within its linear memory, so a compromised runtime corrupts
  only its own memory. But it is **largely redundant given an already-empty jail** — the
  jail assumes the worker is fully compromised, and corrupting a zero-capability worker
  yields control of a process with nothing in it, which was the assumed state already. The
  containment lands where it is least needed, at the cost of double interpretation. Where
  capability sets are *not* empty (the compositor, `navigatord`), no JS runs.
- **A custom bytecode** is strictly worse than WASM: no ecosystem scrutiny, no formal
  semantics, and our own verifier bugs to discover.

**What does pay off is not an execution format at all.** §5.3's tiers 2 and 3 convert JS
into *declarative states* and hash-identified substitutions — removing execution rather
than relocating it. That is where effort aimed at "getting rid of JS" belongs.

**One route is not rejected, only deferred:** changing the *pointer discipline* rather
than the target language. See §11.8.

### 11.8 Deferred, not rejected: a memory-safe engine

Of the five collector bug classes in §11.6, four — missed roots and write barriers, type
confusion while tracing, compaction pointer fixups, refcount errors — are all the same
underlying thing: **references to heap objects that are not correctly tracked.** That
suggests attacking the reference representation itself, and the suggestion is sound.

The mechanism is *not* Rust references: the borrow checker handles static lifetimes and
cannot express "this object is live because a collector says so". The mechanism is the
Rust idiom for graph structures — **arena allocation with generational indices in place of
pointers** — plus type-system-enforced rooting, of which `gc-arena`'s branded lifetimes
(a compile error to hold a GC pointer across a collection point) is the state of the art.

Mapped onto the four classes:

| class | effect of arena + generational indices |
|---|---|
| compaction fixup | **eliminated** — no pointers to fix up; data moves, indices stay valid |
| missed root | **downgraded from security to availability** — a freed-but-indexed object is caught by the generation counter, producing a clean error rather than a use-after-free |
| tracing type confusion | largely eliminated — typed arenas make layout statically known |
| refcount errors | eliminated as a *manual* class — refcounting becomes compiler-managed |

It would additionally close the **buffer-overflow class that §11.6 explicitly cannot**, so
on memory safety this is strictly better than the non-reclaiming arena, not merely
different. And it reclaims, which non-reclaiming by definition does not — so it is the
**only** approach that helps long-running tier-4 apps.

**Why it is deferred rather than adopted:** it cannot be retrofitted. SpiderMonkey's and
QuickJS's object models *are* raw pointers and their own collectors; changing the pointer
discipline is rewriting the core.

**The candidate landscape (surveyed 2026-09-19).** Boa is not the only option, and an
earlier draft of this section badly understated it:

| engine | language | licence | test262 | notes |
|---|---|---|---|---|
| **Boa** | Rust | **Unlicense / MIT** | **~95.5%** (≈51k/53k), 4th on test262.fyi, above JavaScriptCore | `boa_gc` tracing collector. Self-described "experimental". Slower than JIT engines |
| **Nova** | Rust | MPL-2.0 | ~80%; `nova_vm` 1.0.0 in March 2026 | **The architectural match**: data-oriented design, normal Rust enums carrying on-stack data or a 32-bit handle into homogeneous arenas, hot/cold split. Safepoint GC built on reborrowing — type-enforced rooting, the idea above applied to a whole engine. Explicitly "not fast"; gaps include sparse arrays, RegExp lookbehind, WASM |
| **brimstone** | Rust | was copyleft, relicensed — **must be verified** | "effectively feature complete"; ~2× Boa's speed | **Compacting collector written in deliberately unsafe Rust**; its author states moving it to safe Rust is impractical |
| **Kiesel** | Zig | — | — | Zig is not memory-safe in the sense this section requires |

**The evaluation criterion is not the implementation language.** brimstone is the
cautionary case: the fastest of the Rust engines, written in Rust, and yet its collector —
exactly the component this section is about — is deliberately unsafe by design. "Written
in Rust" does not imply the object model and collector are memory-safe. The question to
ask of any candidate is **how much `unsafe` lives in the collector and object model, and
whether it is concentrated and auditable** or spread through the hot path.

**Licensing separates them more sharply than conformance does.** Boa's Unlicense/MIT sits
in LICENSING-POLICY.md's allowed-outright column — the only candidate that needs no
per-component evaluation. Nova and Servo are both MPL-2.0, the "evaluate per-component"
bucket. brimstone's relicensing must be checked before it is considered at all.

**Corrected assessment:** conformance is no longer Boa's blocker — at ~95.5% it is ahead
of a production browser engine. Its gap is *performance*, and §11.4's proving ground is
precisely where performance does not matter. Hence the instrument decision there.

### 11.8.2 Trigger 2 was the wrong measurement

Trigger 2 originally read "the corpus measurement showing tier 4 is material rather than
marginal", where the corpus number is the fraction of *pages* convertible to tiers 1–3.
That metric is wrong, and would have been wrong in a way that hid the answer.

**Page count and user stakes are different distributions.** Somebody may read a hundred
articles and use three applications: the articles dominate any page-weighted number while
the three applications hold the hours and all of the consequences. "95% of pages convert"
can be simultaneously true and completely misleading about whether tier 4 matters. Basing
an engineer-year decision on it would be a dominant-variable error.

The sharpest illustration: **webmail is the root of trust for every other account**, since
password resets land there. "Gmail is in tier 4" is not one application in a weak lane —
it is the recovery path for everything else in the lane with the weakest memory-safety
story. No page-weighted percentage surfaces that.

**Replacement metric: an enumerated list, not a percentage.** For each high-stakes task a
real user performs, ask: does an open protocol exist, could a native jailed app serve it,
or is tier 4 the only path?

| task | open protocol? | native path? | tier 4 required? |
|---|---|---|---|
| mail, calendar, contacts | IMAP / JMAP / SMTP / CalDAV | yes | **no** |
| chat | XMPP / Matrix | yes | **no** |
| banking portals | none | no | **yes** |
| collaborative documents, design tools | none | no | **yes** |
| proprietary SaaS | none | usually no | **yes** |

Two conclusions follow, and they point in opposite directions:

- **"Everyone uses webmail" argues for a native mail client, not a better browser engine.**
  Mail has open protocols and is among the best candidates for exactly what the thesis
  prescribes. Where the highest-stakes item in tier 4 has a native path, the right response
  is to move it *out* of tier 4 rather than to harden tier 4 around it.
- **The residue is what decides the engine question.** Banking is the sharpest case: high
  stakes, no open protocol, and one cannot ask a bank to ship an Atrium app. If that list
  is substantial, tier 4 is load-bearing and §11.9's gap is a principal exposure rather
  than a footnote.

This list is obtainable in days and is far more decision-relevant than a corpus
percentage. The corpus number remains useful for a different question — whether the
*document* lane covers ordinary reading — and should not be retired, only stopped from
answering a question it cannot.

### 11.8.3 The list, filled in for the corpus's applications (2026-11-04)

§11.8.2 asks for an enumerated list. This is it for the application pages in the two
corpora. **It comes from public knowledge of each product, not from measurement.** The
corpus recordings are logged-out landing pages and cannot show the application behind the
sign-in. "Thin" means the logic and data sit behind a server API and the client is
presentation plus sync. "Fat" means the product's logic runs in the client.

| application | client | what the client does | open protocol | path |
|---|---|---|---|---|
| Gmail | thin, large | UI, rich-text compose, IndexedDB cache, push sync, private JSON-RPC | IMAP / JMAP / SMTP | **native mail app** |
| Google Calendar | thin | UI over events | CalDAV | **native** |
| Mastodon | thin | UI over the REST API | ActivityPub + Mastodon API | **native** |
| Bluesky | thin | UI over the API | AT Protocol | **native** |
| Twitch | thin + video | HLS player, chat UI | HLS video; chat is IRC | **native** (watch and chat) |
| WhatsApp Web | **fat** | end-to-end encryption runs in the client, paired to the phone | none (DMA interop pending) | **tier 4**, or nothing |
| Spotify | thin + DRM | player, encrypted audio via EME/Widevine | none | **blocked by DRM**, even in tier 4 |
| Figma | **fat** | C++ to WASM editor on WebGL: document model, layout, rendering | none | **tier 4** (WASM + WebGL), or nothing |
| Notion | medium | block editor and local cache in the client; private sync API | none | reading: tier 1/2 · editing: **tier 4** |
| Trello | thin | board UI, drag and drop, REST API | public REST API | reading: §5.4.5 · full UI: native client on the API, or tier 4 |
| Instagram, Pinterest, TikTok | thin | infinite feed over private APIs; TikTok adds video | none | feeds: §5.4.5 at best (login, anti-bot) · posting: **tier 4** |
| LinkedIn Jobs, Indeed | thin | search and listings | none | **§5.4.5** (search binding) · applying: form template |
| Airbnb, Booking, TripAdvisor | thin + map | search, a WebGL map widget, booking forms | none | search: **§5.4.5** · map: **tier 4** · checkout: see payments |
| Coursera, edX | thin + video | catalogue, HLS/DASH video, quizzes as form submits | none | **§5.4.5** + native video |
| Khan Academy | medium | client-side exercise renderer that grades answers in the page | none | reading: tier 1/2 · exercises: **tier 4** |
| weather.com | thin | forecast data into templates | none (public weather APIs exist) | **§5.4.5** |
| Etsy, Apple Store | thin | catalogue and cart | none | browsing: tier 1/2 + §5.4.5 · checkout: see payments |
| Chase, Vanguard, Citibank | thin, hardened | UI plus fraud and device-fingerprinting scripts | none in the US (Open Banking APIs in the UK and EU need a licensed third party) | **tier 4**, and fraud scoring may challenge it even there |

**What the list says, beyond §11.8.2:**

1. **Two-thirds of these are thin clients.** Their logic is on the server. The client
   mostly turns API responses into a UI, which is §5.4.5's shape (and, where an open
   protocol exists, a native app's). Tier 4 is not the only way in for most of them.
2. **Payments are a cross-cutting tier-4 need.** Card fields are deliberately client-side,
   in cross-origin iframes running the processor's tokenisation JS (Stripe, Adyen,
   3-D Secure), so that the merchant never sees the card number. Every shop and travel
   site meets this at checkout. It is one capability that recurs everywhere, so it is
   worth a dedicated answer, perhaps a native payment sheet, rather than tier 4 per site.
3. **DRM is a separate wall.** Spotify (and Netflix-class video) needs EME with a licensed
   CDM. Tier 4 does not supply one, so these stay out of reach whatever the engine.
4. **The genuinely fat clients are few and specific:** Figma, WhatsApp's client-side
   cryptography, Notion's editor, Khan's exercises, map widgets. These, banking and
   checkout are what tier 4 actually has to carry.

### 11.8.1 Why Boa for the instrument when Nova has the better architecture

Both statements hold, because they answer different questions, and the apparent conflict
dissolves in three steps:

1. **The instrument's job is measurement, not production.** What matters is whether it can
   actually run real pages' JS, what it costs to integrate, and its licence. Performance
   and long-term architecture are close to irrelevant for an offline converter that exists
   to produce one number.
2. **An instrument must not confound what it measures.** This is decisive. At ~80%
   conformance with known gaps in sparse arrays and RegExp lookbehind — both ordinary in
   real-world code — Nova would fail to convert pages for reasons that have nothing to do
   with the pages. We could not then distinguish "this content genuinely resists tier-2
   conversion" from "our tool is missing a feature", and §5.4's corpus number would come
   out systematically pessimistic. We would then decide whether to undertake the single
   largest piece of work in D6 on bad data.
3. **Nova's architectural advantage lands in a lane the instrument is not in.** §11.8's
   case — arena plus generational indices, type-enforced rooting — buys most where memory
   must be *reclaimed*, i.e. long-running tier-4 apps. The instrument is ephemeral and
   offline, so §11.6's non-reclaiming arena already removes the same bug classes there for
   free. **Nova's edge is precisely where non-reclaiming fails, and the instrument is
   precisely where it does not.**

So: Boa now, because the measurement must be trustworthy; Nova watched, because it is
building the right thing for the lane where our cheap trick runs out. The thin engine
boundary in §11.4 is what keeps that from being a fork in the road — re-running the same
instrument on Nova is how trigger 1 gets measured when the time comes.

**The incremental path that makes this tractable.** An immature engine does not have to
start in the hardest lane. **Tier 2 conversion (§4) is the ideal proving ground:** it runs
offline, so performance is nearly irrelevant; it is retryable; and a failure degrades to
"convert with the mature engine instead" rather than breaking someone's browsing. The bar
there is *conformant enough to prerender real pages*, which is dramatically lower than
*run the interactive web*. A memory-safe engine can therefore be adopted or grown in a
place where its weaknesses are cheap, and extended toward tier 4 only as it earns it.

**Re-evaluation triggers**, so that "keep it in mind" is actionable rather than a sentiment:

1. **Boa's conformance against our own tier-2 corpus** reaching the level where it converts
   pages the mature engine converts. Measured on our corpus, not on a published score.
2. **The no-native-path task list (§11.8.2) coming back substantial**, especially if it
   includes high-stakes tasks. This replaces an earlier, wrong formulation of this
   trigger — see below.
3. **A vulnerability class in the adopted engine that the jail does not adequately
   contain** — i.e. one that reaches past a zero-capability worker. By H8.1's reasoning
   that is the point at which engine hardening stops being low-value.
4. **The document lane maturing** to where the engine becomes the weakest link in the
   system rather than one risk among many.

### 11.9 The tier-4 gap, named

Tier 4 is the one lane where almost none of this design's memory-safety work applies, and
that should be stated plainly rather than discovered later.

A full browser engine means Servo, Servo means SpiderMonkey, and SpiderMonkey has exactly
the architecture §11.8 argues against: raw pointers, a tracing collector with rooting
obligations spread through the mutator, and a JIT. Of our mitigations:

| mitigation | tier 2 | tier 4 |
|---|---|---|
| jail (zero-capability worker) | yes | yes — but the jail holds a session (§7.4) |
| JIT off, AOT bytecode (§11.5) | yes | yes |
| non-reclaiming arena (§11.6) | yes | **no — the lane is long-running by definition** |
| memory-safe object model (§11.8) | yes, via Boa | **no** |

Swapping the engine does not rescue it: neither Boa nor Nova is a browser engine, and
replacing SpiderMonkey *inside* Servo is not a drop-in — Servo's DOM objects are traced by
SpiderMonkey's collector and its rooting machinery is SpiderMonkey-specific.

### 11.9.1 What genuinely helps

1. **Session recycling.** The obstacle to non-reclaiming is lifetime, not workload — so
   shorten the lifetime. Restart the engine jail on an interval or a memory threshold and
   reload the page, converting "long-running" into a series of ephemeral sessions and
   making §11.6's non-reclaiming arena viable in tier 4 after all. Browsers already
   discard and reload tabs under memory pressure, and web applications are consequently
   reload-tolerant, because browsers reload them constantly. *Costs:* client-side state
   that was never persisted is lost, so recycling must prefer idle moments and never
   interrupt interaction; and the interval trades user disruption against exposure window.
2. **JIT off by default** (§11.5), which removes the largest single class that does still
   apply here.
3. **Per-origin jails with short lives** — the exposure is bounded to one origin's session,
   which is the data that origin already holds (§7.4).

### 11.9.2 Binding a memory-safe engine into Servo — investigated 2026-09-19

The eventual answer is **a memory-safe JS engine bound into Servo**: a new binding and
rooting layer between two existing Rust codebases, rather than a new engine. Findings from
looking at what that actually entails:

**There is an official upstream initiative, and it is low priority.** Servo has a stated
"bring your own JS engine" goal — a Web-IDL-based interface any bindings layer could
implement, explicitly contemplating V8 and Wasm runtimes as well. It is described as
long-term and low-priority. So this is a direction upstream *wants*, meaning the work is
contributable rather than a fork, but nobody is driving it. We would be.

**Four categories of coupling**, per Servo's own report:

1. Low-level `js::jsapi` calls "peppered" through the script crate, exposing SpiderMonkey
   concepts such as untyped `JSObject` directly.
2. **Garbage-collection integration** — Servo manages *Rust object lifetimes with
   SpiderMonkey's collector* (the Josephine design).
3. **WebIDL codegen** emitting SpiderMonkey-specific glue.
4. The `mozjs` / `mozjs-sys` bindings plus the utilities in
   `components/script/dom/bindings`.

**Category 2 is the crux, and it is also exactly where our benefit lies.** This is not
"swap a VM behind an interface": Servo's DOM objects are owned and traced by SpiderMonkey's
collector, so changing engines means **re-homing the DOM's memory-management model**. That
is the largest and riskiest part of the work — and it is precisely the part that would
deliver the memory-safety property §11.8 is after, because it is the DOM lifetimes that
currently depend on an unsafe collector.

**Servo's own incremental path is the right on-ramp.** Upstream is progressively hiding
SpiderMonkey APIs behind safe idiomatic Rust (the WebGPU refactor replaced unsafe
`JSObject` returns with typed concepts). That work is useful regardless of which engine
ever lands, is upstreamable, and produces the enumeration of SpiderMonkey interactions that
any generic interface must be designed against. It is possible to contribute there and gain
both leverage and knowledge without committing to the whole project.

**The engine-side unknown, which must be settled before anything else:** can the candidate
host *foreign* objects — the DOM — and trace into Rust data? Boa's `boa_gc` has
`Trace`/`Finalize` derives for user types, which is promising but unproven at DOM scale.
Nova's arena-and-handle design is a different shape and may make foreign hosting easier or
harder; unknown. **This is a spike, not a port**, and it gates everything downstream.

**A live data point for why this matters:** Servo 0.3 shipped SpiderMonkey updates *to fix
memory-safety bugs*. The concern is a recurring maintenance reality, not a theoretical one.

**Honest scale:** "less work than writing our own engine" remains true, and should not be
read as "small". Re-homing the DOM lifetime model of a browser engine is a major project.
The argument for it is that it is *bounded, upstreamable, and reuses two mature codebases*,
where writing our own is none of those.

**Recommended next steps, cheap before expensive:**

1. **Unsafe-surface audit** of `boa_gc` and Nova's collector against §11.8's criterion —
   how much `unsafe`, how concentrated, how auditable. Days.
2. **Foreign-object hosting spike** — can Boa hold and trace a non-trivial Rust object
   graph shaped like a DOM subtree? Days to weeks, and it decides whether the candidate is
   viable at all.
3. **Contribute to Servo's de-SpiderMonkey cleanup** — useful regardless, upstreamable,
   and the fastest way to learn the true scale of categories 1 and 3.
4. Only then estimate category 2 with evidence rather than a guess.

### 11.9.3 Placement does not rescue this, because we do not control the servers

atrium-navigator.md §3.2's JS-free-client invariant puts the legacy engine *server-side*,
which appears to move this gap off the user's machine. It does not, in general, because
that clause means **vendor-hosted** (insula.md §0.6.4) — and the sites that most need tier
4 are precisely the ones that will never cooperate. One cannot ask a bank to host Servo.

The honest placement matrix:

| who runs the engine jail | works for | privacy cost | where the gap sits |
|---|---|---|---|
| **the site's vendor** | only cooperating sites | none | vendor's server |
| **an operator-run rendering proxy** (ours or a provider's) | any site | **sees all browsing** | operator's server |
| **the user's own always-on machine** (home server, desktop serving a phone) | any site | none — self-hosted | user's own infrastructure |
| **localhost jail** (default fallback) | any site | none | **the user's device** |

**Policy: never default to an operator-run rendering proxy.** It works for every site and
is genuinely useful for thin clients, but it reintroduces exactly the third party this
architecture exists to remove — an operator who sees every page a user visits. It may be
offered; it must be an explicit, informed opt-in, never a default and never silent.

So the working answer for arbitrary existing sites is a **localhost jail by default**,
with self-hosted placement on a machine the user controls as the better option where one
exists — which is a real option rather than a hypothetical, since location transparency
makes it the same binary either way.

Which means §11.9's gap does sit on the user's device in the default configuration. What
bounds it is the jail, JIT-off (§11.5) and session recycling (§11.9.1) — not placement.

### 11.9.4 The adoption risk, stated

The native model is a **proposal to an ecosystem that may not adopt it**. That risk is
taken deliberately, and it is worth being explicit about how it is bounded:

- **Open protocols need nobody's permission.** Mail, calendar, contacts and chat can be
  served by native jailed apps today, against IMAP/JMAP/CalDAV/XMPP/Matrix. The
  highest-stakes category (§11.8.2) is reachable with zero site cooperation, which makes
  it the natural beachhead rather than a later phase.
- **Servers under our control can serve the native way immediately**, demonstrating the
  model end-to-end without waiting for anyone.
- **The adoption risk is therefore concentrated in proprietary SaaS** — which is exactly
  the residue that needs tier 4. The risk is real, but it is a known and enumerable set
  rather than "the whole web".
- **Tiers 1–3 must be good enough that adoption is not required for the system to be
  worth using.** If reading the web works well without any site changing anything, the
  proposal gets time to be adopted or not.

### 11.9.5 Why this is tolerable in the meantime

Not because the risk is small, but because of where it sits:

- **Tier 4 is the explicitly-marked legacy lane, and the lane is visible** (§8). Its weaker
  posture is disclosed and chosen, not hidden behind a uniform claim of safety — and
  §11.9.3 means that disclosure has to include *where the engine is running*.
- **The thesis already answers "I want a secure interactive app": make it a native jailed
  app.** Tier 4 is a bridge for content that predates that answer, not the future the
  architecture is arguing for.
- **Whether it matters at all is a measurement, not a guess.** §5.4's corpus number decides
  both whether tier 4 gets built and whether its architecture is a rounding error or the
  system's principal exposure. If the legacy lane turns out to carry real traffic, §11.8's
  trigger 2 fires and this stops being tolerable — by design.

## 12. Open questions

1. **Zygote fork cost under Portcullis** — measured, not assumed, and it decides §7.6's
   origin-vs-site granularity.
2. **Carrying the engine's accessibility tree into our semantic tree** — feasibility is a
   port question and determines whether the legacy lane keeps G5.
3. **Freshness for prerendered artifacts** — a Nomenclator question (backend spec §10.3).
4. **Whether the declarative vocabulary of §5 is v1 or v2** of the document profile; it is
   not needed for reading, but it is needed before anyone authors anything interactive.
5. **Remote placement policy** — when, if ever, a remote engine jail is offered by default,
   given it reintroduces a third party who sees browsing.
6. ~~**Whether challenged sites are in scope at all**~~ — **settled in §5.4.1b**: serve the
   open web automatically and the challenged fraction by reader-assisted import; identify
   honestly and never impersonate a browser; surface a refusal instead of converting the
   challenge page. What remains open is the *detection* rule for marking an artifact
   REFUSED-BY-ORIGIN, which is a matter of evidence rather than principle.
7. ~~**Where the data-access boundary sits for hydrating pages**~~ — **settled in
   §5.4.1c**: the page network stays same-origin GET. Measured, the same-site relaxation
   buys one marginal document, rescues none from demotion, and admits 12x the traffic —
   989 requests of which come from one document to one first-party host and produce no
   content at all.
8. **Whether conversion can record data, input and time as well as states** — proposed in
   §5.4.5 (data bindings inferred by replaying perturbed copies of the prerender's own
   responses, input handlers classified onto constraint validation and request templates,
   commuting transitions recorded alone, timer cycles driven natively). Open until the
   coverage measurement and the one-page prototype of §5.4.5.6 report.
