
/**
 * Create a project, or rename one. The id is never an input: it is minted on
 * create and immutable afterwards.
 */
export interface ProjectInput {
  name: string;
}