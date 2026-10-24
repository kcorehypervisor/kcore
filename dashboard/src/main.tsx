import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { Code, ConnectError } from "@connectrpc/connect";
import { router } from "./router";
import { SessionGate } from "./session";
import "./styles.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: false,
      retry: (failures, error) =>
        failures < 1 && !(error instanceof ConnectError && [Code.Unauthenticated, Code.PermissionDenied, Code.InvalidArgument, Code.NotFound, Code.Unimplemented].includes(error.code)),
      staleTime: 15_000,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <SessionGate>
        <RouterProvider router={router} />
      </SessionGate>
    </QueryClientProvider>
  </StrictMode>,
);
