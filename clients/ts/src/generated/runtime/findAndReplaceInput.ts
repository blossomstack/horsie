
export interface FindAndReplaceInput {
  path: string;
  find: string;
  replace: string;
  regex?: boolean;
  replaceAll?: boolean;
}