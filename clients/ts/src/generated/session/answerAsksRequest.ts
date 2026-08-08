
import { AskAnswerInput } from './askAnswerInput';
/**
 * Answers to every question one agent is parked on, delivered together
 */
export interface AnswerAsksRequest {
  answers: AskAnswerInput[];
}