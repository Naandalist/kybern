// Spaces group projects (work, personal…). The daemon stores the groups and
// announces every change with `spaces.changed`; which space is active is this
// phone's choice, remembered per computer. Shared rules (`inSpace`, colours,
// copy, the default space) live in `packages/kybern-client/src/spaces.ts`.
import { setSpacePlacements, spaceProject } from "../../../../packages/kybern-client/src/spacePlacements";
import {
  canDeleteSpace,
  inSpace,
  nextSpaceColor,
  SPACE_COPY,
} from "../../../../packages/kybern-client/src/spaces";
import * as SecureStore from "expo-secure-store";
import { useMemo, useSyncExternalStore } from "react";
import { Platform } from "react-native";
import { METHOD_NOT_FOUND } from "./notesModel";
import {
  isFreeChatProject,
  SPACES_CHANGED_NOTIFICATION,
  type Project,
  type Space,
  type SpaceColor,
  type SpaceId,
  type SpacesChangedNotification,
} from "./protocol";
import {
  activeEnvironment,
  currentClient,
  machineHub,
  mutate,
  onClientAttach,
  onEnvironmentReset,
  onSubscriptionReady,
  rpc,
} from "./runtime";

import { useLists } from "./useLists";

export { canDeleteSpace, inSpace, nextSpaceColor, SPACE_COPY };

type SpacesState = {
  /** null until the first `spaces.list` answers; false on daemons without spaces. */
  supported: boolean | null;
  spaces: Space[];
  /** The space this phone shows, as saved. `currentSpaceId()` resolves a stale one. */
  savedId: SpaceId | null;
};

let state: SpacesState = { supported: null, spaces: [], savedId: null };
const listeners = new Set<() => void>();
function publish(patch: Partial<SpacesState>) {
  state = { ...state, ...patch };
  listeners.forEach((fn) => fn());
}
function subscribe(fn: () => void) {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}
const read = () => state;

/** Spaces arrive in rail order; keep them sorted however they came. */
const ordered = (spaces: readonly Space[]) => [...spaces].sort((a, b) => a.position - b.position);

const savedKey = () => {
  const id = activeEnvironment()?.id;
  // SecureStore keys allow [A-Za-z0-9._-].
  return id ? `kybern.ink.space.${id.replace(/[^A-Za-z0-9._-]/g, "")}` : null;
};

onClientAttach((client, isCurrent) => {
  client.onNotification(SPACES_CHANGED_NOTIFICATION, (params) => {
    if (!isCurrent()) return;
    const spaces = (params as SpacesChangedNotification | null)?.spaces;
    if (Array.isArray(spaces)) { publish({ supported: true, spaces: ordered(spaces) }); void loadSpaces(); }
  });
  client.onNotification("spaces.placements_changed", () => { if (isCurrent()) void loadSpaces(); });
  const key = savedKey();
  if (key && Platform.OS !== "web")
    void SecureStore.getItemAsync(key)
      .then((value) => {
        if (isCurrent() && value && !state.savedId) publish({ savedId: value });
      })
      .catch(() => {});
});
onSubscriptionReady(() => { networkSpacesFrom = null; void loadSpaces(); });
onEnvironmentReset(() => { spacesLoadVersion++; networkSpacesFrom = null; setSpacePlacements([]); publish({ supported: null, spaces: [], savedId: null }); });

/** The member the spaces were last read from while the computer the phone is at was away. */
let networkSpacesFrom: unknown = null;
/**
 * While the computer the phone is at is away and another machine of the network answers, read the spaces from that
 * member: members keep the same spaces, so Home keeps its space switcher. Once per member; the computer's own list
 * replaces it when it answers again.
 */
export function loadSpacesThroughNetwork() {
  if (currentClient()?.status === "open") return;
  const client = machineHub().memberClient();
  if (!client || networkSpacesFrom === client) return;
  networkSpacesFrom = client;
  const version = ++spacesLoadVersion;
  client.call("spaces.list", {}).then(
    (result) => {
      if (version !== spacesLoadVersion) return;
      setSpacePlacements(result.spaces, result.placements);
      publish({ supported: true, spaces: ordered(result.spaces) });
    },
    (e) => {
      if (version !== spacesLoadVersion) return;
      if ((e as { code?: number } | null)?.code === METHOD_NOT_FOUND) publish({ supported: false, spaces: [] });
      else networkSpacesFrom = null;
    },
  );
}

let spacesLoadVersion = 0;
export async function loadSpaces() {
  const version = ++spacesLoadVersion;
  try {
    const result = await rpc("spaces.list", {});
    if (version !== spacesLoadVersion) return;
    setSpacePlacements(result.spaces, result.placements);
    publish({ supported: true, spaces: ordered(result.spaces) });
  } catch (e) {
    if (version !== spacesLoadVersion) return;
    if ((e as { code?: number } | null)?.code === METHOD_NOT_FOUND)
      publish({ supported: false, spaces: [] });
  }
}

/** The active space's id; null when the daemon has no spaces (show everything). */
export function currentSpaceId(s: SpacesState = state): SpaceId | null {
  if (s.supported === false) return null;
  if (s.supported === null) return s.savedId;
  if (s.savedId && s.spaces.some((space) => space.id === s.savedId)) return s.savedId;
  return s.spaces.find((space) => space.is_default)?.id ?? s.spaces[0]?.id ?? null;
}

export function useSpaces() {
  return useSyncExternalStore(subscribe, read, read);
}

/** The active space, or null when the daemon has none. */
export function useCurrentSpace(): Space | null {
  const s = useSpaces();
  const id = currentSpaceId(s);
  return s.spaces.find((space) => space.id === id) ?? null;
}

/** Switch the space this phone shows. Nothing animates: lists simply show the other space. */
export function setCurrentSpace(id: SpaceId) {
  if (id === currentSpaceId()) return;
  publish({ savedId: id });
  const key = savedKey();
  if (key && Platform.OS !== "web") void SecureStore.setItemAsync(key, id).catch(() => {});
}

/** The projects of the active space (every project when the daemon has no spaces). */
export function useSpaceProjects(): Project[] {
  const { projects } = useLists();
  const s = useSpaces();
  const id = currentSpaceId(s);
  return useMemo(
    () => (id === null ? projects : projects.filter((p) => isFreeChatProject(p.id) || p.space_id === id)),
    [projects, id],
  );
}

/** A predicate for items with a project: true when they belong to the active space. */
export function useInSpace() {
  const { projects } = useLists();
  const id = currentSpaceId(useSpaces());
  return useMemo(
    () => (projectId: string | null | undefined) => inSpace(projectId, projects, id),
    [projects, id],
  );
}

// ---- changes (the daemon answers with `spaces.changed` too; publishing here makes them instant) ----

function upsert(space: Space) {
  publish({ spaces: ordered([...state.spaces.filter((s) => s.id !== space.id), space]) });
}

export async function createSpace(name: string, color?: SpaceColor) {
  const space = await rpc("spaces.create", { name: name.trim(), color: color ?? nextSpaceColor(state.spaces) });
  upsert(space);
  return space;
}

export async function updateSpace(id: SpaceId, patch: { name?: string; color?: SpaceColor }) {
  const space = await rpc("spaces.update", { space_id: id, ...patch, ...(patch.name ? { name: patch.name.trim() } : {}) });
  upsert(space);
  return space;
}

/** Deletes a space; its projects move to the space the daemon names. The default space can't be deleted. */
export async function deleteSpace(id: SpaceId) {
  const result = await rpc("spaces.delete", { space_id: id });
  publish({
    spaces: state.spaces.filter((s) => s.id !== id),
    ...(state.savedId === id ? { savedId: result.moved_to } : {}),
  });
  return result;
}

export async function moveSpace(id: SpaceId, offset: -1 | 1) {
  const ids = state.spaces.map((s) => s.id);
  const from = ids.indexOf(id);
  const to = from + offset;
  if (from < 0 || to < 0 || to >= ids.length) return;
  ids.splice(to, 0, ...ids.splice(from, 1));
  const result = await rpc("spaces.reorder", { space_ids: ids });
  publish({ spaces: ordered(result.spaces) });
}

/** Moves a project (with its threads, notes and tasks) to another space. */
export async function moveProjectToSpace(projectId: string, spaceId: SpaceId) {
  const hub = machineHub();
  const logical = hub.view().projects.find((p) => p.instances.some((i) => i.project.id === projectId));
  const instance = logical?.instances.find((i) => i.project.id === projectId) ?? logical?.primary;
  if (instance) {
    try {
      const result = await rpc("spaces.place", { project: spaceProject(instance.machineId, instance.project), space_id: spaceId }, hub.primaryMachineId ?? undefined);
      setSpacePlacements(result.spaces, result.placements);
      publish({ spaces: ordered(result.spaces) });
      return result;
    } catch (error) {
      if ((error as { code?: number })?.code !== METHOD_NOT_FOUND) throw error;
    }
  }
  return mutate("projects.update", { project_id: projectId, space_id: spaceId });
}
