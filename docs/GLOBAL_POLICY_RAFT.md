# Global policy Raft membership

Global-policy voters have two separate properties: whether they contribute a
vote to quorum, and whether they are eligible to become leader. Eligibility is
bootstrap topology, not an ordinary Raft policy record.

For example, a deployment can use leader-eligible voters in the US and UK and
a voting-only voter in the fictional region of Pottsylvania:

```text
us@10.0.0.11:9910#leader,uk@10.0.0.12:9910#leader,pottsylvania@10.0.0.13:9910#voter
```

The Pottsylvania member is a blind voting witness. It stores only term, index,
and salted commitment metadata and can vote for the US or UK candidate. It does
not receive plaintext policy commands or state snapshots. It never campaigns,
cannot receive a vote, and is rejected if it presents itself as leader. If
neither approved candidate is available, the cluster intentionally has no
leader even when the Pottsylvania voter remains online.

A blind witness is not a recoverable copy of a policy command. An acknowledged
mutation therefore requires two leader-eligible full replicas as well as the
ordinary Raft majority. With either US or UK unavailable, the remaining full
replica and Pottsylvania may elect a leader and preserve current read authority,
but policy mutation pauses. Write availability in that state requires a future
encrypted-witness-log mode whose ciphertext can be recovered and decrypted by
either trusted full replica.

Full replicas must use mutually authenticated, encrypted transport so the
blind witness cannot observe their plaintext replication traffic. Data-plane
traffic and encryption keys never traverse the witness protocol.

Leader ineligibility is a jurisdictional placement control, not a Byzantine
consensus mechanism. A region that cannot be trusted with replicated policy or
with correct Raft behavior must not be a voter at all; connect it through the
scoped federation APIs instead.

## Multiple federations per region

A physical region may run members of several global federations. Each
federation is an independent Raft group with its own federation identifier,
peer set, persisted state file, management credential, policy revisions, trust
grants, links, and leases. Identifiers such as `shared-link` are scoped by that
boundary and may safely repeat in another federation.

Every RPC carries the expected federation identifier. Management status and
mutation requests also require the federation's management credential. A node
rejects a mismatched federation before dispatch and rejects a missing or wrong
credential before exposing status or proposing a log entry. Persisted state is
bound to its federation identifier and cannot be opened as another federation.

The global RPC transport encrypts and authenticates the entire management and
consensus document with the federation's rotating credential; there is no
plaintext compatibility fallback. The optional `native-aead+tls` mode adds
mutually authenticated TLS 1.3 without replacing native encryption. The shared
management credential is still an interim federation-wide authorization
boundary rather than a principal identity. Production employee isolation also
requires separate management and consensus listeners, certificate identities
bound to node/principal authorization, and audit records. Host root remains
able to inspect every federation process and credential co-located on that
host; federations that do not trust the same host administrator must not be
co-located.

The credential is stored as an expiring, overlapping version bundle rather
than an indefinite token. Rotation, native encrypted framing, TLS, and test
controls are documented in `GLOBAL_TRANSPORT_SECURITY.md`.

The five-VM overlapping-membership simulation is run with:

```text
scripts/zcglobal-multifederation-qemu.sh
```

It exercises three independent three-voter groups across US, UK, and fictional
Pottsylvanian PoPs; reciprocal foreign status and unlink attempts; colliding
record identifiers; and ciphertext-only placement without key escrow. It is a
control-plane isolation test and does not claim to exercise encrypted volume
I/O or data-plane deletion.
