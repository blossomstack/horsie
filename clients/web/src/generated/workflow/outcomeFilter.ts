
import { OutcomeIn } from './outcomeIn';
import { OutcomeNotIn } from './outcomeNotIn';
/**
 * Which outcomes an edge is taken for.
 *
 * Two operators, not four: equality is a one-element `in`, and inequality a
 * one-element `not_in`. Every value must be one the producing step declares,
 * which is checked when the workflow is saved — so a mistyped outcome is a
 * rejected form rather than a run that ends halfway.
 */
export type OutcomeFilter =
  | { op: "In"; value: OutcomeIn }
  | { op: "NotIn"; value: OutcomeNotIn };