# zcutils

Zero-copy stream tools, lane-multiplexed TCP transport, encryption filters and
Linux storage/network experiments. Start with the tools and their contracts in
the [repository guide](../README.md).

## Looking for Diskpipe?

Visit the [Diskpipe documentation](../zccusan/docs/site/index.md) for the storage
area network, volume lifecycle, replication and failover. Choose
[Diskpipe with Kubernetes](../zccusan/docs/site/kubernetes.md) or
[Diskpipe without Kubernetes](../zccusan/docs/site/standalone.md).

## Tools and contracts

- [Commands, builds and examples](../README.md): the umbrella binary and direct pipeline commands.
- [Descriptor protocol](../docs/zero-copy-descriptor.md): descriptor-native transport and its implementation boundaries.
- [Zero Copy Cinematic Universe](universe.md): how the related projects fit together.

## Identity laboratories

These shared debug integrations exercise issuer boundaries, scoped permissions,
supplementary approvals and revocation. The storage operating procedures belong
to [Diskpipe day-two operations](../zccusan/docs/DAY_2_FEDERATED_IDENTITY_AND_FAILOVER.md).

- [SAML and real-time revocation](../docs/saml-realtime-revocation-debug.md): an OAuth-backed IdP and two nginx auth proxies.
- [Federated identity laboratory](../docs/federated-identity-breakglass-debug.md): independent OAuth issuers and time-limited approval.

The laboratories describe their local evidence and production limitations.
Local exercises create no AWS resources; opt-in real AWS checks have separate
cost and teardown instructions.
