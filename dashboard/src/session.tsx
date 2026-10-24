import { createContext, useCallback, useContext, useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { auth, errorMessage, tokenStore } from "./rpc";
import { rowOf, type Row } from "./rows";
import { Login } from "./Login";

type Session = {
  operator: string;
  role: string;
  roles: string[];
  method: string;
  expiresAt: string;
  logout: () => Promise<void>;
  refresh: () => Promise<void>;
};

const SessionContext = createContext<Session | null>(null);

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error("useSession outside SessionGate");
  return session;
}

export function SessionGate({ children }: { children: ReactNode }) {
  const token = useSyncExternalStore(tokenStore.subscribe, tokenStore.get, () => "");
  const queryClient = useQueryClient();
  const [who, setWho] = useState<Row | null>(null);
  const [checking, setChecking] = useState(Boolean(token));
  const [notice, setNotice] = useState("");

  const refresh = useCallback(async () => {
    if (!tokenStore.get()) {
      setWho(null);
      return;
    }
    setChecking(true);
    try {
      setWho(rowOf(await auth.whoAmI({})));
    } catch (error) {
      setWho(null);
      tokenStore.set("");
      setNotice(errorMessage(error));
    } finally {
      setChecking(false);
    }
  }, []);

  useEffect(() => {
    if (token) {
      refresh();
    } else {
      setWho((current) => {
        if (current) setNotice("Your session ended. Sign in again.");
        return null;
      });
      setChecking(false);
      queryClient.clear();
    }
  }, [token, refresh, queryClient]);

  const logout = useCallback(async () => {
    try {
      await auth.logout({});
    } catch {
      /* the token is dropped either way */
    }
    setNotice("");
    tokenStore.set("");
  }, []);

  if (checking) {
    return (
      <div className="login">
        <p className="muted">Checking your session…</p>
      </div>
    );
  }
  if (!token || !who) {
    return (
      <Login
        notice={notice}
        onSession={(next) => {
          setNotice("");
          tokenStore.set(next);
        }}
      />
    );
  }
  const session: Session = {
    operator: String(who.operator ?? ""),
    role: String(who.effective_role ?? ""),
    roles: Array.isArray(who.roles) ? who.roles.map(String) : [],
    method: String(who.method ?? ""),
    expiresAt: String(who.expires_at ?? ""),
    logout,
    refresh,
  };
  return <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
}
