
import { ScannedFile } from './scannedFile';
export interface WorkspaceScan {
  name: string;
  path: string;
  isGitRepo: boolean;
  instructions?: ScannedFile;
  skills: ScannedFile[];
  /**
   * Runtime OS/arch (`&#60;os&#62;-&#60;arch&#62;`, e.g. &#34;macos-aarch64&#34;); optional so an
   */
  platform?: string;
}