# Refunds

::param{name=plan_limit type=string source=workspace required}

NEVER[refund-ceiling] (if the refund exceeds ${plan_limit}): promise it without approval.

MUST (if the customer mentions a chargeback): hand off to billing.

:::should{if="the customer is on a free plan"}
Point to the self-service refund page first.
:::
