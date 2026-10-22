import { useEffect, useMemo, useState } from "react";
import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { api, type Catalog, type Resource } from "./api";

function groupResources(resources: Resource[]) {
  const groups: { name: string; items: Resource[] }[] = [];
  for (const resource of resources) {
    const found = groups.find((group) => group.name === resource.group);
    if (found) found.items.push(resource);
    else groups.push({ name: resource.group, items: [resource] });
  }
  return groups;
}

export function Shell() {
  const path = useRouterState({ select: (state) => state.location.pathname });
  const [navOpen, setNavOpen] = useState(false);
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api<Catalog>("/api/v1/catalog"),
  });
  const groups = useMemo(() => groupResources(catalog.data?.resources ?? []), [catalog.data]);

  useEffect(() => {
    setNavOpen(false);
  }, [path]);

  return (
    <div className="app">
      <nav id="nav" className={navOpen ? "open" : ""}>
        <div className="brand">
          kcore <span>dashboard</span>
        </div>
        <Link to="/" className={path === "/" ? "active" : ""}>
          Overview
        </Link>
        {groups.map((group) => (
          <div key={group.name}>
            <div className="group">{group.name}</div>
            {group.items.map((resource) => {
              const href = `/r/${resource.id}`;
              return (
                <Link key={resource.id} to="/r/$id" params={{ id: resource.id }} className={path === href ? "active" : ""}>
                  {resource.title}
                </Link>
              );
            })}
          </div>
        ))}
        {catalog.isError ? <p className="error">{(catalog.error as Error).message}</p> : null}
      </nav>
      <div className="workspace">
        <header className="mobile-bar">
          <button
            type="button"
            className="menu"
            aria-expanded={navOpen}
            aria-controls="nav"
            onClick={() => setNavOpen((open) => !open)}
          >
            {navOpen ? "Close" : "Menu"}
          </button>
          <div className="brand">
            kcore <span>dashboard</span>
          </div>
        </header>
        <main>
          <Outlet />
        </main>
      </div>
    </div>
  );
}
