import React, { createContext, useContext } from 'react';
import type { Account, Settings } from './types';

export interface AppContextValue {
  settings: Settings;
  account: Account | null;
  saveSettings: (patch: Partial<Settings>) => Promise<void>;
  reloadSettings: () => Promise<void>;
  navigate: (to: string) => void;
  route: Route;
  toast: (text: string, error?: boolean) => void;
  launch: (options: Record<string, any>) => Promise<void>;
  refreshAccount: () => Promise<void>;
  openSearch: () => void;
}

export interface Route {
  name: string;
  id: string;
  query: URLSearchParams;
}

export const AppContext = createContext<AppContextValue | null>(null);

export function useApp(): AppContextValue {
  const value = useContext(AppContext);
  if (!value) throw new Error('AppContext missing');
  return value;
}
