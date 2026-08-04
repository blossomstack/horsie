
import { ScannedFile } from './scannedFile';
export interface WorkspaceScan {
  name: string;
  path: string;
  isGitRepo: boolean;
  instructions?: ScannedFile;
  skills: ScannedFile[];
  /**
   * Runtime OS/arch (`&lt;os&gt;-&lt;arch&gt;`, e.g. &quot;macos-aarch64&quot;); optional so an
   */
  platform?: string;
}