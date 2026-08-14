
/**
 * The runtime refused this request outright.
 *
 * A reply any waiter accepts, because several requests have no error shape of
 * their own: a scan answers with skills, a hook run with records, a discovery
 * with tools, and none of them can say "no". Without this the only ways to
 * refuse one were to answer it wrongly — which reads as a protocol confusion —
 * or to answer it emptily, which is indistinguishable from a legitimate empty
 * result and is how a sequencing bug becomes a model that has quietly lost its
 * skills.
 *
 * Named for what it is rather than for its one caller: refusing a request that
 * names an unprovisioned agent is the first reason, not the only possible one.
 *
 * `RequestRefused`, not `RequestFailed`: fluorite resolves imported types by
 * bare name across packages, and `runtime_vendor` already has a `RequestFailed`.
 * A second one silently hijacks the union arm to the wrong type rather than
 * erroring.
 */
export interface RequestRefused {
  callId: string;
  reason: string;
}