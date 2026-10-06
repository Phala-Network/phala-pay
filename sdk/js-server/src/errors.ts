/** Errors contain no request bodies, credentials, or raw transport causes. */
export class PhalaPayError extends Error {
  constructor(message: string) {
    super(
      message
        .replace(/ppay_(?:sk|rk)_(?:test|live)_[A-Za-z0-9]+/g, "[redacted]")
        .replace(/(?:qt|da)_[A-Za-z0-9]+_secret_[A-Za-z0-9]+/g, "[redacted]"),
    );
    this.name = new.target.name;
  }
}
export class ConfigurationError extends PhalaPayError {}
export class ResponseValidationError extends PhalaPayError {
  constructor(
    message = "Invalid service response",
    readonly statusCode: number | null = null,
    readonly requestId: string | null = null,
  ) {
    super(message);
  }
}
export class AddressMismatchError extends ResponseValidationError {}
export class AttestationError extends PhalaPayError {}
export class SignatureVerificationError extends PhalaPayError {}
export class LedgerSnapshotError extends PhalaPayError {}
export class TransportError extends PhalaPayError {
  constructor(readonly code: "network" | "timeout" | "cancelled") {
    super(`Request ${code}`);
  }
}
export class ApiError extends PhalaPayError {
  constructor(
    readonly statusCode: number,
    readonly code: string,
    message: string,
    readonly errorType: string | null,
    readonly param: string | null,
    readonly docUrl: string | null,
    readonly requestId: string | null,
    readonly retryAfter: number | null,
  ) {
    super(message);
  }
}
