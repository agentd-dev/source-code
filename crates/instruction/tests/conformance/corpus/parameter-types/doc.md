# Typed parameters

:::param[]
| name    | type     | values      | default | example          | source    |
|---------|----------|-------------|---------|------------------|-----------|
| limit   | number   |             | 500     | 750              | workspace |
| docs    | url      |             |         | https://docs.test | workspace |
| wait    | duration |             | 15m     |                  | workspace |
| tier    | enum     | free, pro   |         |                  | workspace |
| regions | list     |             |         |                  | workspace |
:::

Limit ${limit}; docs at ${docs}; escalate after ${wait}; tier ${tier}; regions ${regions}.
