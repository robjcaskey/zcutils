#!/bin/sh
echo "ZCGLOBAL_REGIONAL_LEAF_FAILURE role=${ZCGLOBAL_ROLE:-unknown} action=power-loss" >/dev/console
echo OK
# Close the one-shot control connection before removing the guest NIC.  The
# delayed child still models abrupt data-plane power loss, while the injector
# receives a deterministic complete acknowledgement rather than waiting for a
# dead TCP peer's retransmission timeout.
( sleep 0.05; poweroff -f ) </dev/null >/dev/null 2>&1 &
exit 0
