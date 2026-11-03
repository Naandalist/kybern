// Stand-ins for the native modules `state/runtime.ts` imports, for the tests that run it in Node.
// SecureStore keeps its items in `globalThis.__secure` (a Map the test seeds and reads);
// the React hooks come from the test (`globalThis.reconnectSubscribe`).
globalThis.__secure ??= new Map();
const sources = {
  "react-native": 'export const Platform = { OS: "android" }; export const AppState = { addEventListener() {} };',
  "expo-secure-store": `export const WHEN_UNLOCKED_THIS_DEVICE_ONLY = "test";
    export async function setItemAsync(key, value) { globalThis.__secure.set(key, value); }
    export async function getItemAsync(key) { return globalThis.__secure.get(key) ?? null; }
    export async function deleteItemAsync(key) { globalThis.__secure.delete(key); }`,
  "expo-file-system": `export const Paths = { document: "/doc" };
    export class File { constructor() { this.exists = false; } create() {} write() {} textSync() { return ""; } }`,
  "expo-crypto": "export function getRandomValues(bytes) { bytes.fill(7); return bytes; }",
  "expo/fetch": `export async function fetch(url, init) {
      (globalThis.__fetches ??= []).push({ url, init });
      return { ok: globalThis.__fetchOk !== false, json: async () => ({ id: "a1", name: "pic.png", media_type: "image/png", size: 3 }) };
    }`,
  react: "export const useCallback = fn => fn; export const useMemo = fn => fn(); export const useEffect = () => {}; export const useSyncExternalStore = (...args) => globalThis.reconnectSubscribe?.(...args) ?? args[1]();",
};
export const nativeModuleSource = (specifier) => sources[specifier] ?? null;
export const seedSecure = (items) => { globalThis.__secure.clear(); for (const [k, v] of Object.entries(items)) globalThis.__secure.set(k, v); };
