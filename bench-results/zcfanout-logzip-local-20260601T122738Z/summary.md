| mode | result rec/s | emitted IOPS | logical 4K IOPS | branch Gbit/s |
|---|---:|---:|---:|---:|
| mirror-write | 1213295627 | 606647814 | 606647814 | 39757.271 |
| mirror-read | 1525070685 | 762535342 | 762535342 | 49973.516 |
| stripe-read | 1409045828 | 704522914 | 704522914 | 23085.807 |

Larger materialized-log runs:

| mode | result records | emitted records | result rec/s | emitted IOPS | logical 4K IOPS | logical payload covered |
|---|---:|---:|---:|---:|---:|---:|
| mirror-write | 64000000 | 32000000 | 1569780817 | 784890408 | 784890408 | 51438.578 Gbit/s |
| stripe-read | 64000000 | 32000000 | 1677615511 | 838807755 | 838807755 | 27486.053 Gbit/s |

Notes:
- This is descriptor/result-log zipper speed only. It does not send TCP, touch block devices, or copy payload bytes.
- `logical 4K IOPS` is the amount of 4K work represented by descriptors, not measured memory payload bandwidth.
- The useful signal is that monotonic result-log zipping is cheap enough that the fast fanout design should spend its budget on RX/TX zero-copy, payload lifetime, and log credit/backpressure rather than request sorting or per-request waits.
