import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "@tanstack/react-router";
import { Code, ConnectError } from "@connectrpc/connect";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { consoleController, errorMessage } from "./rpc";

type State = "connecting" | "open" | "closed" | "missing";

const STATUS: Record<State, string> = {
  connecting: "Connecting through the controller…",
  open: "Connected. Keys go to the guest's serial port.",
  closed: "Disconnected.",
  missing: "There is no VM with this name.",
};

export function ConsolePage() {
  const { vm } = useParams({ from: "/console/$vm" });
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<State>("connecting");
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!host.current) return;
    const term = new Terminal({
      cursorBlink: true,
      convertEol: false,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 14,
      theme: { background: "#0a0a0a" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();
    term.focus();
    const onResize = () => fit.fit();
    window.addEventListener("resize", onResize);

    const abort = new AbortController();
    const encoder = new TextEncoder();
    let session = "";
    let queue: Promise<unknown> = Promise.resolve();
    setState("connecting");
    setError("");

    const input = term.onData((data) => {
      if (!session) return;
      const bytes = encoder.encode(data);
      const id = session;
      queue = queue
        .then(() => consoleController.sendVmConsoleInput({ sessionId: id, data: bytes }, { signal: abort.signal }))
        .catch((err) => {
          if (!abort.signal.aborted) setError(errorMessage(err));
        });
    });

    (async () => {
      try {
        for await (const event of consoleController.openVmConsole({ vmName: vm }, { signal: abort.signal })) {
          if (event.sessionId) {
            session = event.sessionId;
            setState("open");
          }
          if (event.data.length) term.write(event.data);
        }
        if (!abort.signal.aborted) {
          term.write("\r\n\x1b[2m[console closed]\x1b[0m\r\n");
          setState("closed");
        }
      } catch (err) {
        if (abort.signal.aborted) return;
        setError(errorMessage(err));
        setState(ConnectError.from(err).code === Code.NotFound ? "missing" : "closed");
      }
    })();

    return () => {
      abort.abort();
      input.dispose();
      window.removeEventListener("resize", onResize);
      term.dispose();
    };
  }, [vm, attempt]);

  return (
    <div className="stack console-page">
      <header className="top">
        <div>
          <p className="eyebrow">Serial console</p>
          <h1>{vm}</h1>
          <p className="muted">
            {STATUS[state]}
          </p>
        </div>
        <div className="buttons">
          {state === "closed" ? (
            <button type="button" className="primary" onClick={() => setAttempt((current) => current + 1)}>
              Reconnect
            </button>
          ) : null}
          <Link to="/r/$id" params={{ id: "vms" }} search={{ q: "" }} className={state === "missing" ? "primary" : undefined}>
            Back to VMs
          </Link>
        </div>
      </header>
      {error && state !== "missing" ? <p className="error">{error}</p> : null}
      <div className="terminal" ref={host} hidden={state === "missing"} />
    </div>
  );
}
