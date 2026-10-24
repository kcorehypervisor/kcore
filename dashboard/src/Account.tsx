import { useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import QRCode from "qrcode";
import { auth, errorMessage } from "./rpc";
import { useSession } from "./session";
import { formatDate, fullDate } from "./format";

export function AccountPage() {
  const session = useSession();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [repeat, setRepeat] = useState("");
  const [message, setMessage] = useState<{ tone: "ok" | "bad"; text: string } | null>(null);
  const [enroll, setEnroll] = useState<{ qr: string; secret: string } | null>(null);
  const [code, setCode] = useState("");
  const [codes, setCodes] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);

  async function guarded(work: () => Promise<void>) {
    setBusy(true);
    setMessage(null);
    try {
      await work();
    } catch (error) {
      setMessage({ tone: "bad", text: errorMessage(error) });
    } finally {
      setBusy(false);
    }
  }

  function changePassword(event: FormEvent) {
    event.preventDefault();
    if (next !== repeat) {
      setMessage({ tone: "bad", text: "The new passwords do not match." });
      return;
    }
    guarded(async () => {
      const resp = await auth.changePassword({ currentPassword: current, newPassword: next });
      setCurrent("");
      setNext("");
      setRepeat("");
      setMessage({ tone: "ok", text: `Password changed. ${resp.sessionsRevoked} other session(s) signed out.` });
    });
  }

  function beginEnroll() {
    guarded(async () => {
      const resp = await auth.beginTotpEnrollment({});
      setEnroll({ qr: await QRCode.toDataURL(resp.otpauthUri, { margin: 1, width: 200 }), secret: resp.secret });
      setCodes([]);
    });
  }

  function confirmEnroll(event: FormEvent) {
    event.preventDefault();
    guarded(async () => {
      const resp = await auth.confirmTotpEnrollment({ totpCode: code.replace(/\s+/g, "") });
      setEnroll(null);
      setCode("");
      setCodes(resp.recoveryCodes);
      setMessage({ tone: "ok", text: "The new authenticator is active. The old one and the old recovery codes no longer work." });
    });
  }

  return (
    <div className="stack">
      <header className="top">
        <div>
          <p className="eyebrow">Identity</p>
          <h1>Account</h1>
        </div>
      </header>
      {message ? <p className={message.tone === "ok" ? "banner ok" : "error"}>{message.text}</p> : null}
      <div className="split">
        <section className="card stack">
          <h2>Signed in</h2>
          <dl className="facts">
            <div>
              <dt>Operator</dt>
              <dd>{session.operator}</dd>
            </div>
            <div>
              <dt>Role in effect</dt>
              <dd>{session.role || "None"}</dd>
            </div>
            <div>
              <dt>Roles</dt>
              <dd>{session.roles.join(", ") || "None"}</dd>
            </div>
            <div>
              <dt>Session ends</dt>
              <dd>{session.expiresAt ? <time dateTime={session.expiresAt} title={fullDate(session.expiresAt)}>{formatDate(session.expiresAt)}</time> : "—"}</dd>
            </div>
          </dl>
          <div className="quick">
            <Link to="/r/$id" params={{ id: "sessions" }} search={{ q: "" }}>
              My sessions
            </Link>
            <Link to="/r/$id" params={{ id: "api-tokens" }} search={{ q: "" }}>
              API tokens
            </Link>
          </div>
        </section>
        <form className="card stack" onSubmit={changePassword}>
          <h2>Change password</h2>
          <label>
            Current password
            <input type="password" autoComplete="current-password" value={current} onChange={(event) => setCurrent(event.target.value)} required />
          </label>
          <label>
            New password
            <input type="password" autoComplete="new-password" minLength={12} value={next} onChange={(event) => setNext(event.target.value)} required />
          </label>
          <label>
            Repeat new password
            <input type="password" autoComplete="new-password" minLength={12} value={repeat} onChange={(event) => setRepeat(event.target.value)} required />
          </label>
          <p className="muted">At least 12 characters. Your other sessions are signed out.</p>
          <button type="submit" className="primary" disabled={busy}>
            Change password
          </button>
        </form>
      </div>
      <section className="card stack">
        <h2>Authenticator</h2>
        {!enroll ? (
          <>
            <p className="muted">Move two-factor to a new device. The current one keeps working until you confirm the new code.</p>
            <div className="buttons">
              <button type="button" onClick={beginEnroll} disabled={busy}>
                Replace authenticator
              </button>
            </div>
          </>
        ) : (
          <form className="stack" onSubmit={confirmEnroll}>
            <img className="qr" src={enroll.qr} alt="Authenticator QR code" width={200} height={200} />
            <code className="secret">{enroll.secret.replace(/(.{4})/g, "$1 ").trim()}</code>
            <label>
              Code from the new device
              <input inputMode="numeric" autoComplete="one-time-code" value={code} onChange={(event) => setCode(event.target.value)} required />
            </label>
            <div className="buttons">
              <button type="button" onClick={() => setEnroll(null)}>
                Cancel
              </button>
              <button type="submit" className="primary" disabled={busy || !code}>
                Confirm
              </button>
            </div>
          </form>
        )}
        {codes.length ? (
          <>
            <p>New recovery codes. They are not shown again.</p>
            <pre className="code recovery">{codes.join("\n")}</pre>
          </>
        ) : null}
      </section>
    </div>
  );
}
