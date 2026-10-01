const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$/;
const ADDRESS = /^[A-Za-z0-9._:-]{1,253}$/;
const SHA256 = /^[0-9a-fA-F]{64}$/;
const DISK = /^\/dev\/[A-Za-z0-9/_-]{1,120}$/;
const SIZE = /^[1-9][0-9]{0,18}([KMGT]i?B?|B)?$/i;

export class InputError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "InputError";
  }
}

function rejectControlChars(value: string, label: string): void {
  if (value.includes("\0") || /[\r\n]/.test(value)) {
    throw new InputError(`${label} contains a newline or null byte`);
  }
}

export function assertName(value: string, label: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, label);
  if (!NAME.test(trimmed)) {
    throw new InputError(
      `${label} must start with a letter or digit and contain only letters, digits, '.', '_' or '-'`,
    );
  }
  return trimmed;
}

export function assertAddress(value: string, label: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, label);
  if (trimmed.startsWith("-") || !ADDRESS.test(trimmed) || !trimmed.includes(":")) {
    throw new InputError(`${label} must look like host:port`);
  }
  return trimmed;
}

export function assertHttpsUrl(value: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, "image URL");
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    throw new InputError("image URL must be an https:// address");
  }
  if (url.protocol !== "https:") {
    throw new InputError("image URL must use https://");
  }
  return trimmed;
}

export function assertSha256(value: string): string {
  const trimmed = value.trim();
  if (!SHA256.test(trimmed)) {
    throw new InputError("image SHA256 must be 64 hexadecimal characters");
  }
  return trimmed.toLowerCase();
}

export function assertDisk(value: string, label: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, label);
  if (!DISK.test(trimmed)) {
    throw new InputError(`${label} must be a /dev/ path`);
  }
  return trimmed;
}

export function assertSize(value: string | number, label: string): string {
  const text = String(value).trim();
  rejectControlChars(text, label);
  if (!SIZE.test(text) && !/^[1-9][0-9]{0,18}$/.test(text)) {
    throw new InputError(`${label} must be a byte count or a size such as 40G`);
  }
  return text;
}

export function assertSshPublicKey(value: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, "SSH public key");
  if (!trimmed.startsWith("ssh-")) {
    throw new InputError("SSH public key must start with ssh-");
  }
  if (trimmed.length > 8192) {
    throw new InputError("SSH public key is too long");
  }
  return trimmed;
}

export function assertArg(value: string, label: string): string {
  const trimmed = value.trim();
  rejectControlChars(trimmed, label);
  if (trimmed.startsWith("-") || trimmed.length === 0 || trimmed.length > 4096) {
    throw new InputError(`${label} is empty or looks like a flag`);
  }
  return trimmed;
}

export function isBlank(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.trim().length === 0;
  if (Array.isArray(value)) return value.length === 0;
  return false;
}
