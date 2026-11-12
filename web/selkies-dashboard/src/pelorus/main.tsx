/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Entry point of the Pelorus page, served at `pelorus/` where the server has
 * Pelorus on: the chat alone, filling the window, with no stream beside it.
 * The page sits one directory under the client's root, so the API it talks to
 * is `api` relative to its own address, whatever prefix the server is under.
 * @module
 */
import React from "react";
import ReactDOM from "react-dom/client";
import { ThemeProvider } from "@/components/ui/theme-provider";
import { PelorusChat } from "@/pelorus/chat";
import "../index.css";

const mount = document.getElementById("root");
if (mount) {
  ReactDOM.createRoot(mount).render(
    <React.StrictMode>
      <ThemeProvider defaultTheme="dark" storageKey="theme">
        <div className="h-dvh w-full bg-background text-foreground">
          <PelorusChat base="api" standalone />
        </div>
      </ThemeProvider>
    </React.StrictMode>,
  );
}
