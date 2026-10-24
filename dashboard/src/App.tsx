import { useEffect, useMemo, useState } from "react";
import { Link, Outlet, useNavigate, useRouterState } from "@tanstack/react-router";
import * as Dialog from "@radix-ui/react-dialog";
import { RESOURCES, type Resource } from "./catalog";
import { displayTitle, HIDDEN_NAV } from "./format";
import { Logo } from "./Logo";
import { useSession } from "./session";

function groupResources(resources: Resource[]) {
  const groups: { name: string; items: Resource[] }[] = [];
  for (const resource of resources) {
    if (HIDDEN_NAV.has(resource.id)) continue;
    const found = groups.find((group) => group.name === resource.group);
    if (found) found.items.push(resource);
    else groups.push({ name: resource.group, items: [resource] });
  }
  return groups;
}

const ICONS: Record<string, string> = {
  Compute: "M4 7h16v10H4zM8 7V4h8v3M8 17v3h8v-3",
  Network: "M4 8h6v8H4zM14 8h6v8h-6zM10 12h4",
  Storage: "M4 7c0-1.6 3.6-3 8-3s8 1.4 8 3v10c0 1.6-3.6 3-8 3s-8-1.4-8-3V7zM4 12c0 1.6 3.6 3 8 3s8-1.4 8-3",
  Cluster: "M12 3l8 4.5v6c0 4.5-3.4 7.2-8 8.5C7.4 20.7 4 18 4 13.5v-6L12 3z",
  Identity: "M12 12a3.2 3.2 0 1 0 0-6.4 3.2 3.2 0 0 0 0 6.4zM5.5 19.5c1.2-2.8 3.6-4 6.5-4s5.3 1.2 6.5 4",
  Services: "M4 7h16v12H4zM8 7V4h8v3",
  Observe: "M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
  Search: "M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16zM21 21l-4.3-4.3",
};

function Glyph({ name }: { name: string }) {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
      <path d={ICONS[name] ?? ICONS.Observe} />
    </svg>
  );
}

function readClosed(): Record<string, boolean> {
  try {
    const parsed = JSON.parse(localStorage.getItem("kcore-nav-groups") ?? "{}") as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) return parsed as Record<string, boolean>;
  } catch {
    /* ignore broken local storage */
  }
  return {};
}

export function Shell() {
  const path = useRouterState({ select: (state) => state.location.pathname });
  const navigate = useNavigate();
  const [navOpen, setNavOpen] = useState(false);
  const [palette, setPalette] = useState(false);
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const [closed, setClosed] = useState<Record<string, boolean>>(readClosed);
  const session = useSession();
  const groups = useMemo(() => groupResources(RESOURCES), []);
  const destinations = useMemo(() => {
    const items = [
      { href: "/", title: "Overview", group: "Cluster", id: "" },
      { href: "/account", title: "Account", group: "Identity", id: "" },
    ];
    for (const group of groups) {
      for (const resource of group.items) {
        items.push({ href: `/r/${resource.id}`, title: displayTitle(resource.id, resource.title), group: group.name, id: resource.id });
      }
    }
    const needle = query.trim().toLowerCase();
    return needle ? items.filter((item) => `${item.title} ${item.group}`.toLowerCase().includes(needle)) : items;
  }, [groups, query]);

  useEffect(() => {
    setNavOpen(false);
    setPalette(false);
  }, [path]);

  useEffect(() => {
    setCursor(0);
  }, [query, palette]);

  useEffect(() => {
    const group = groups.find((item) => item.items.some((resource) => path === `/r/${resource.id}`));
    if (!group) return;
    setClosed((current) => {
      if (!current[group.name]) return current;
      const next = { ...current, [group.name]: false };
      localStorage.setItem("kcore-nav-groups", JSON.stringify(next));
      return next;
    });
  }, [path, groups]);

  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPalette(true);
        setNavOpen(true);
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  function toggleGroup(name: string) {
    setClosed((current) => {
      const next = { ...current, [name]: !current[name] };
      localStorage.setItem("kcore-nav-groups", JSON.stringify(next));
      return next;
    });
  }

  function go(href: string) {
    setPalette(false);
    if (href === "/") navigate({ to: "/" });
    else if (href === "/account") navigate({ to: "/account" });
    else navigate({ to: "/r/$id", params: { id: href.replace("/r/", "") }, search: { q: "" } });
  }

  return (
    <div className="app">
      <a className="skip" href="#content">
        Skip to content
      </a>
      <nav id="nav" className={navOpen ? "open" : ""}>
        <div className="brand">
          <Logo id="mark-nav" />
          <span>
            kcore <span>dashboard</span>
          </span>
        </div>
        <button type="button" className="goto" onClick={() => setPalette(true)}>
          <Glyph name="Search" />
          Go to…
          <kbd>Ctrl K</kbd>
        </button>
        <Link to="/" className={path === "/" ? "active" : ""} aria-current={path === "/" ? "page" : undefined}>
          Overview
        </Link>
        {groups.map((group) => (
          <div className="section" key={group.name}>
            <button type="button" className="group" aria-expanded={!closed[group.name]} onClick={() => toggleGroup(group.name)}>
              <Glyph name={group.name} />
              <span>{group.name}</span>
              <span className="chevron" aria-hidden="true">{closed[group.name] ? "▸" : "▾"}</span>
            </button>
            {closed[group.name]
              ? null
              : group.items.map((resource) => {
                  const href = `/r/${resource.id}`;
                  return (
                    <Link key={resource.id} to="/r/$id" params={{ id: resource.id }} search={{ q: "" }} className={path === href ? "active" : ""} aria-current={path === href ? "page" : undefined}>
                      {displayTitle(resource.id, resource.title)}
                    </Link>
                  );
                })}
          </div>
        ))}
        <div className="who">
          <Link to="/account" className={path === "/account" ? "active" : ""}>
            <strong>{session.operator}</strong>
            <span className="muted">{session.role || "no role"}</span>
          </Link>
          <button type="button" className="ghost" onClick={() => session.logout()}>
            Sign out
          </button>
        </div>
      </nav>
      <div className="workspace">
        <header className="mobile-bar">
          <button type="button" className="menu" aria-expanded={navOpen} aria-controls="nav" onClick={() => setNavOpen((open) => !open)}>
            {navOpen ? "Close" : "Menu"}
          </button>
          <div className="brand">
            <Logo id="mark-bar" />
            <span>
              kcore <span>dashboard</span>
            </span>
          </div>
        </header>
        <main id="content">
          <Outlet />
        </main>
      </div>
      <Dialog.Root open={palette} onOpenChange={setPalette}>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className="dialog palette">
            <Dialog.Title className="sr">Go to</Dialog.Title>
            <input
              autoFocus
              placeholder="Go to a page"
              aria-label="Go to a page"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setCursor((current) => Math.min(destinations.length - 1, current + 1));
                } else if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setCursor((current) => Math.max(0, current - 1));
                } else if (event.key === "Enter" && destinations[cursor]) {
                  event.preventDefault();
                  go(destinations[cursor].href);
                }
              }}
            />
            <div className="palette-list" role="listbox">
              {destinations.map((item, index) => (
                <button type="button" key={item.href} className={index === cursor ? "active" : ""} onMouseEnter={() => setCursor(index)} onClick={() => go(item.href)}>
                  <span>{item.title}</span>
                  <span className="muted">{item.group}</span>
                </button>
              ))}
              {destinations.length === 0 ? <p className="muted">No matching page.</p> : null}
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  );
}
