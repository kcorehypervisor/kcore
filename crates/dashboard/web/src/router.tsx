import { createRootRoute, createRoute, createRouter } from "@tanstack/react-router";
import { Shell } from "./App";
import { Overview, ResourcePage } from "./pages";

const root = createRootRoute({ component: Shell });
const overview = createRoute({ getParentRoute: () => root, path: "/", component: Overview });
const resource = createRoute({ getParentRoute: () => root, path: "/r/$id", component: ResourcePage });

export const router = createRouter({
  routeTree: root.addChildren([overview, resource]),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
