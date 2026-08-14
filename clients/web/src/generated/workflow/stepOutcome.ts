
/**
 * One value a step's `outcome` may take, and what choosing it means.
 *
 * The description is not decoration: it is what the model reads to pick
 * between values, and it is the only thing standing between "failure" meaning
 * *the work failed* and meaning *I could not finish*.
 */
export interface StepOutcome {
  value: string;
  description: string;
}