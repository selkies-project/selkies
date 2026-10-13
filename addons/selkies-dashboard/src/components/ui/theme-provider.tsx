/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The color mode on the document: the shared `ui_mode` setting
 * (`src/appearance.ts`) applied as the `dark` or `light` class Tailwind's
 * variant reads, and as the `theme-dark` or `theme-light` class the classic
 * dashboard's widgets (the gamepad visualizer) read.
 * @module
 */

import { createContext, useContext, useEffect } from "react"
import { drawnMode, setMode, useAppearance } from "@/appearance"

type ThemeProviderState = {
  /** The mode picked: dark, light, or system. */
  theme: string
  /** Picks a mode. */
  setTheme: (theme: string) => void
}

const ThemeProviderContext = createContext<ThemeProviderState>({ theme: "dark", setTheme: () => null })

/** Applies the mode in force to `html` and hands the pick to the tree. */
export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const { mode } = useAppearance()
  const resolved = drawnMode()

  useEffect(() => {
    const root = window.document.documentElement
    root.classList.remove("light", "dark", "theme-light", "theme-dark")
    root.classList.add(resolved, `theme-${resolved}`)
  }, [resolved])

  return (
    <ThemeProviderContext.Provider value={{ theme: mode, setTheme: setMode }}>
      {children}
    </ThemeProviderContext.Provider>
  )
}

/** The mode picked and its setter. */
export const useTheme = () => useContext(ThemeProviderContext)
