import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { SessionsLayout } from "./pages/SessionsLayout";
import { SessionView } from "./pages/SessionView";
import { Welcome } from "./pages/Welcome";

const client = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

export default function App() {
  return (
    <QueryClientProvider client={client}>
      <BrowserRouter>
        <Routes>
          <Route path="/" element={<SessionsLayout />}>
            <Route index element={<Welcome />} />
            <Route path="sessions/:id" element={<SessionView />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
