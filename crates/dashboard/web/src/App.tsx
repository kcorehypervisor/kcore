import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { api, type Catalog } from "./api";

export function Shell() {
  const path = useRouterState({ select: (state) => state.location.pathname });
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api<Catalog>("/api/v1/catalog"),
  });
  const resources = catalog.data?.resources ?? [];
  const groups = [...new Set(resources.map((resource) => resource.group))];

  return (
    <div className="app">
      <nav>
        <div className="brand">
          kcore <span>dashboard</span>
        </div>
        <Link to="/" className={path === "/" ? "active" : ""}>
          Overview
        </Link>
        {groups.map((group) => (
          <div key={group}>
            <div className="group">{group}</div>
            {resources
              .filter((resource) => resource.group === group)
              .map((resource) => {
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
      <main>
        <Outlet />
      </main>
    </div>
  );
}
