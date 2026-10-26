import { createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Shell } from "./App";
import { AccountPage } from "./Account";
import { ConsolePage } from "./Console";
import { Overview, ResourcePage } from "./pages";

const root = createRootRoute({ component: Shell });
const overview = createRoute({ getParentRoute: () => root, path: "/", component: Overview });
const resource = createRoute({
  getParentRoute: () => root,
  path: "/r/$id",
  validateSearch: (search: Record<string, unknown>) => ({
    q: typeof search.q === "string" ? search.q : "",
  }),
  component: ResourcePage,
});
const consoleRoute = createRoute({ getParentRoute: () => root, path: "/console/$vm", component: ConsolePage });
const account = createRoute({ getParentRoute: () => root, path: "/account", component: AccountPage });

export const router = createRouter({
  routeTree: root.addChildren([overview, resource, consoleRoute, account]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
