
import { ScannedFile } from './scannedFile';
export interface WorkspaceScan {
  name: string;
  path: string;
  isGitRepo: boolean;
  instructions?: ScannedFile;
  skills: ScannedFile[];
  /**
   * Runtime OS/arch (`<os>-<arch>`, e.g. "macos-aarch64"); optional so an
   * older runtime binary still deserializes against a newer server.
   */
  platform?: string;
}