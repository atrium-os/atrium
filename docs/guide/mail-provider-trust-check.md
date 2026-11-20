# Mail SMTP and mailbox-write qualification

Qualify the intended Work deployment against the [Mail provider-result rules](../spec/mail.md#7-reading). Keep provider trust disabled in Mail throughout the run. The parser replay below is an isolated diagnostic. A passing run supplies evidence for a separate decision about account trust.

## Before the run

- Have the receiving operator confirm the dedicated test Inbox, normal external SMTP ingress, enabled inbound connectors and the trusted `authserv-id` from deployment configuration. Record the source of that identity before inspecting any test message.
- Confirm the Mail read path and arrange a separate authenticated writer for every enabled mailbox-write path. List the exact operations and destination folders with the receiving operator. Record disabled paths with the operator's confirmation.
- Agree on a UTC test window and the approved controls with both operators. Use synthetic messages addressed only to the test Inbox, and assign a unique case label and Message-ID for each attempt.
- Prepare a fresh Mail index for each control or destination-path check. Record the Mail build revision and confirm that provider trust is disabled before syncing.

## External SMTP controls

| Case | Procedure | Required evidence and result |
| --- | --- | --- |
| SPF-only positive | The relay operator sends a message with a single From mailbox through normal external ingress. It must pass DMARC through aligned SPF with no aligned DKIM signature. Fetch the original Internet message through the same IMAP or JMAP path Mail will use. | Match the sender's SMTP outcome, receiver trace and Inbox copy by Message-ID and the receiver's queue or network ID when available. Confirm that the receiver supplied the first `Authentication-Results` field with the configured identity, exactly one `dmarc=pass`, and a matching `header.from`. With trust disabled, the normal Mail index and opened message show the sender as unverified. |
| Ingress negative | The operators run the agreed control for an outside claim to the receiving service identity, with no aligned authentication. | Record the SMTP response and receiving trace. If delivered, fetch the original bytes and check that the receiving service removed or replaced the outside claim and that there is no trusted pass. If rejected or quarantined, record the receiving disposition and confirm there is no Inbox delivery. If quarantine release is an enabled path in scope, check the released copy too. |

For the positive, replay the exact fetched bytes in an isolated parser check with the independently confirmed identity and again with the identity unset. Record the parser verdicts. Expect provider-attributed verification only in the first run. Use the parser-worker integration test `provider_verdict_requires_parent_supplied_identity_and_one_from_mailbox` as the reference for the diagnostic exchange. The checked-in test itself uses synthetic input.

Retain the relay’s test fixture or final outbound header record with the SMTP response. Compare it with the fetched copy to show which authentication field was added at ingress. An SMTP acceptance response shows that a server accepted responsibility for the message. Require the receiving trace and the fetched Inbox copy to claim delivery. If multiple enabled routes can deliver to this mailbox, record coverage per route.

## Mailbox-write controls

1. Use a separate authenticated client to put the agreed synthetic test message with an untrusted authentication claim into the test Inbox by each enabled write path. Start with IMAP APPEND. Check IMAP COPY and MOVE from another folder independently where enabled. For JMAP, check import into Inbox separately from import into another folder followed by a mailbox change. Include enabled provider-specific and client bulk-import paths.
2. Record the operation, source folder if applicable, destination and server status. Record the destination UID and UIDVALIDITY or the corresponding JMAP or provider message ID when available. Check whether unsuccessful writes left any matching Inbox copy.
3. For each successful write, sync with a fresh Mail index and open the resulting message. Match it to the write result. Capture the sender verdict from both the stored summary and reader and inspect the original fetched bytes, or the protected delivery metadata if the deployment provides it.
4. Record whether the received claim survived and whether the same trusted result used for the SMTP case was available on the mailbox-written copy. A surviving matching header disqualifies header-only provider trust even if Mail currently shows the sender as unverified. Separately protected metadata needs its own provenance check against both SMTP and mailbox-write cases.

## Evidence and decision

Keep the exact fetched bytes for synthetic cases in the approved evidence location so the parser checks can be repeated. Record their SHA-256 digests. For a rejection without a fetched message, record the receiving-side reason and the evidence for absence from Inbox. Keep credentials, tokens and unrelated mailbox contents out of logs. Use evidence references in the handoff.

Use one row per attempt.

| Case and UTC time | Ingress route or write path | Message-ID and server IDs | SMTP or write outcome | Receiver trace or destination evidence | Fixture and fetched bytes hashes, authentication result | Index, reader and parser verdicts | Disposition |
| --- | --- | --- | --- | --- | --- | --- | --- |
| | | | | | | | |

Use `pass`, `fail`, `blocked`, or `disabled` for each row. Record why a row is blocked or disabled. A missing trace, an untested enabled path, or a positive control that also passes local DKIM leaves the deployment unqualified. At closeout, record the covered routes and paths, failures and gaps, evidence location, and next owner in the Mail handoff. Keep provider trust disabled.
