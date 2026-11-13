import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { CastBar } from "./cast/CastBar";
import { DevicesPage } from "./devices/DevicesPage";
import { DownloadsPage } from "./downloads/DownloadsPage";
import { Home } from "./home/Home";
import "./index.css";
import { useRemote } from "./lib/remote";
import { isTv } from "./lib/tv";
import { Player } from "./player/Player";
import { TitlePlayer } from "./player/TitlePlayer";
import { Link, useRoute } from "./router";
import { SearchPage } from "./search/SearchPage";
import { TitlePage } from "./titles/TitlePage";

function App() {
  const route = useRoute();
  useRemote(route);
  switch (route.page) {
    case "home":
      return <Home query={route.query} />;
    case "title":
      return <TitlePage key={route.id} id={route.id} season={route.season} sources={route.sources} />;
    case "play":
      return <TitlePlayer key={`${route.video}:${route.source}`} video={route.video} source={route.source} />;
    case "torrents":
      return <SearchPage query={route.query} everything={route.everything} />;
    case "watch":
      return <Player key={`${route.ref}:${route.file}`} target={route.ref} file={route.file} />;
    case "downloads":
      return <DownloadsPage />;
    case "devices":
      return <DevicesPage />;
    case "missing":
      return (
        <main className="mx-auto flex min-h-dvh max-w-4xl flex-col items-start justify-center gap-2 px-page">
          <h1 className="text-lg font-medium text-ink">There is no page here</h1>
          <Link href="/" className="relative hit-area rounded-sm text-sm text-action hover:text-ink">
            Go to search
          </Link>
        </main>
      );
  }
}

document.documentElement.toggleAttribute("data-tv", isTv());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
    <CastBar />
  </StrictMode>,
);
