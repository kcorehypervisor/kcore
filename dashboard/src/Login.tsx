import { useEffect, useState, type FormEvent } from "react";
import QRCode from "qrcode";
import { LoginStep } from "./gen/controller_pb";
import { auth, errorMessage, setPendingToken } from "./rpc";
import { Logo } from "./Logo";

type Step =
  | { kind: "password" }
  | { kind: "totp" }
  | { kind: "enroll"; secret: string; uri: string; qr: string; account: string }
  | { kind: "recovery"; codes: string[]; session: string };

const CODE = /^\d{6,8}$/;

export function Login({ notice, onSession }: { notice: string; onSession: (token: string) => void }) {
  const [step, setStep] = useState<Step>({ kind: "password" });
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [useRecovery, setUseRecovery] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);

  useEffect(() => () => setPendingToken(""), []);

  function restart(message: string) {
    setPendingToken("");
    setStep({ kind: "password" });
    setCode("");
    setPassword("");
    setError(message);
  }

  async function guarded(work: () => Promise<void>) {
    setBusy(true);
    setError("");
    try {
      await work();
    } catch (err) {
      const message = errorMessage(err);
      if (step.kind !== "password" && /expired|pending|unauthenticated|sign in/i.test(message)) restart(`${message} Start again.`);
      else setError(message);
    } finally {
      setBusy(false);
    }
  }

  function submitPassword(event: FormEvent) {
    event.preventDefault();
    guarded(async () => {
      const resp = await auth.login({ username: username.trim(), password });
      setPassword("");
      setPendingToken(resp.pendingToken);
      if (resp.nextStep === LoginStep.ENROLL_TOTP) {
        const begin = await auth.beginTotpEnrollment({});
        const qr = await QRCode.toDataURL(begin.otpauthUri, { margin: 1, width: 220 });
        setStep({ kind: "enroll", secret: begin.secret, uri: begin.otpauthUri, qr, account: begin.account });
      } else {
        setStep({ kind: "totp" });
      }
    });
  }

  function submitCode(event: FormEvent) {
    event.preventDefault();
    const value = code.replace(/\s+/g, "");
    if (!useRecovery && !CODE.test(value)) {
      setError("Enter the code from your authenticator app.");
      return;
    }
    guarded(async () => {
      if (step.kind === "enroll") {
        const resp = await auth.confirmTotpEnrollment({ totpCode: value });
        setPendingToken("");
        if (!resp.session?.token) throw new Error("The controller did not start a session.");
        setStep({ kind: "recovery", codes: resp.recoveryCodes, session: resp.session.token });
        return;
      }
      const resp = await auth.verifySecondFactor(useRecovery ? { recoveryCode: value } : { totpCode: value });
      setPendingToken("");
      if (!resp.session?.token) throw new Error("The controller did not start a session.");
      onSession(resp.session.token);
    });
  }

  return (
    <div className="login">
      <div className="card login-card stack">
        <div className="brand">
          <Logo id="mark-login" />
          <span>
            kcore <span>dashboard</span>
          </span>
        </div>
        {notice && step.kind === "password" ? <p className="banner warn">{notice}</p> : null}
        {step.kind === "password" ? (
          <form className="stack" onSubmit={submitPassword}>
            <h1>Sign in</h1>
            <label>
              Operator
              <input autoFocus autoComplete="username" value={username} onChange={(event) => setUsername(event.target.value)} required />
            </label>
            <label>
              Password
              <input type="password" autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} required />
            </label>
            <button type="submit" className="primary" disabled={busy || !username || !password}>
              {busy ? "Signing in…" : "Continue"}
            </button>
            <p className="muted">
              No password yet? A cluster admin sets one with <code>kctl operator set-password &lt;name&gt;</code>.
            </p>
          </form>
        ) : null}
        {step.kind === "totp" ? (
          <form className="stack" onSubmit={submitCode}>
            <h1>Two-factor check</h1>
            <label>
              {useRecovery ? "Recovery code" : "Authenticator code"}
              <input
                autoFocus
                inputMode={useRecovery ? "text" : "numeric"}
                autoComplete="one-time-code"
                value={code}
                onChange={(event) => setCode(event.target.value)}
                required
              />
            </label>
            <button type="submit" className="primary" disabled={busy || !code}>
              {busy ? "Checking…" : "Sign in"}
            </button>
            <button type="button" className="ghost" onClick={() => { setUseRecovery((current) => !current); setCode(""); }}>
              {useRecovery ? "Use the authenticator app" : "Use a recovery code"}
            </button>
            <button type="button" className="ghost" onClick={() => restart("")}>
              Back
            </button>
          </form>
        ) : null}
        {step.kind === "enroll" ? (
          <form className="stack" onSubmit={submitCode}>
            <h1>Set up two-factor</h1>
            <p>Two-factor sign-in is required. Scan this code with an authenticator app, then enter the six-digit code it shows.</p>
            <img className="qr" src={step.qr} alt={`Authenticator QR code for ${step.account}`} width={220} height={220} />
            <details>
              <summary>Enter the key by hand</summary>
              <code className="secret">{step.secret.replace(/(.{4})/g, "$1 ").trim()}</code>
            </details>
            <label>
              Authenticator code
              <input autoFocus inputMode="numeric" autoComplete="one-time-code" value={code} onChange={(event) => setCode(event.target.value)} required />
            </label>
            <button type="submit" className="primary" disabled={busy || !code}>
              {busy ? "Checking…" : "Turn on two-factor"}
            </button>
            <button type="button" className="ghost" onClick={() => restart("")}>
              Cancel
            </button>
          </form>
        ) : null}
        {step.kind === "recovery" ? (
          <div className="stack">
            <h1>Save your recovery codes</h1>
            <p>Each code signs you in once if you lose the authenticator. They are not shown again.</p>
            <pre className="code recovery">{step.codes.join("\n")}</pre>
            <div className="buttons">
              <button type="button" onClick={() => navigator.clipboard?.writeText(step.codes.join("\n"))}>
                Copy
              </button>
              <button
                type="button"
                onClick={() => {
                  const blob = new Blob([`${step.codes.join("\n")}\n`], { type: "text/plain" });
                  const link = document.createElement("a");
                  link.href = URL.createObjectURL(blob);
                  link.download = "kcore-recovery-codes.txt";
                  link.click();
                  URL.revokeObjectURL(link.href);
                }}
              >
                Download
              </button>
            </div>
            <label className="check">
              <input type="checkbox" checked={saved} onChange={(event) => setSaved(event.target.checked)} />I saved these codes
            </label>
            <button type="button" className="primary" disabled={!saved} onClick={() => onSession(step.session)}>
              Open the dashboard
            </button>
          </div>
        ) : null}
        {error ? (
          <p className="error" role="alert">
            {error}
          </p>
        ) : null}
      </div>
    </div>
  );
}
