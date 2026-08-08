
/**
 * A registered session group. Groups may exist with zero sessions; a group
 * referenced only by annotations is not registered but still lists.
 */
export interface SessionGroupView {
  name: string;
}