import { Stack, type ErrorBoundaryProps } from "expo-router";
import { MachinesSettings } from "../src/features/settings/Machines";
import { errorText } from "../src/state/runtime";
import { ErrorBanner, Page } from "../src/ui/primitives";
import { useTheme } from "../src/ui/theme";

export default function Machines() {
  const { colors } = useTheme();
  return (
    <Page grouped>
      <Stack.Screen options={{ title: "Machines", headerStyle: { backgroundColor: colors.grouped } }} />
      <MachinesSettings />
    </Page>
  );
}

/** A machine in an unexpected state must not blank the page (or close the app): say so, with the header to go back. */
export function ErrorBoundary({ error, retry }: ErrorBoundaryProps) {
  const { colors } = useTheme();
  return (
    <Page grouped>
      <Stack.Screen options={{ title: "Machines", headerStyle: { backgroundColor: colors.grouped } }} />
      <ErrorBanner error={`Unable to show your machines: ${errorText(error)}`} onRetry={() => void retry()} />
    </Page>
  );
}
