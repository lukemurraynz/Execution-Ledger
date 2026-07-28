import { useCallback } from "react";

interface CommandAction {
  id: string;
  label: string;
  section: string;
  shortcut?: string;
  action: () => void;
}

export function useCommandPalette(_actions: CommandAction[]) {
  return useCallback(() => {
    // palette rendered via React state in App.tsx, not imperative
  }, []);
}
