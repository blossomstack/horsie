
import { AskAnswerInput } from './askAnswerInput';
/**
 * Answers to every pending ask of a session, delivered together
 */
export interface AnswerAsksRequest {
  answers: AskAnswerInput[];
}