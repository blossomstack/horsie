
import { ContentPart } from './contentPart';
import { Role } from './role';
/**
 * A single message in the conversation
 */
export interface Message {
  id: string;
  role: Role;
  parts: ContentPart[];
}