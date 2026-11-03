// kybern-keychain: the one process that reads and writes Kybern GPUIX's Keychain items on macOS.
//
// The login keychain stamps every item with the creating program's partition, `cdhash:<hash>` for any app without an
// Apple-issued Team ID, and asks for the keychain password whenever a program with another cdhash reads it. The app's
// own executable changes with every release, so the app never touches the Keychain itself: it runs this helper, whose
// bytes (and therefore cdhash) stay the same from release to release. Do not rebuild or re-sign the committed binary
// unless you mean to: every user is asked once more afterwards (README.md in this folder).
//
// The contract is `kybern credentials` (crates/kybern-cli/src/credentials.rs), so the app talks to both the same way:
//   kybern-keychain credentials get|set|delete --service <service> --account <account>
// `set` reads the secret from standard input; `get` writes it to standard output without a newline. The secret is
// never an argument or an error message. Exit codes: 0 done, 1 failed, 2 usage, 3 no such item, 4 locked or declined.
#include <Security/Security.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum { DONE = 0, FAILED = 1, USAGE = 2, NOT_FOUND = 3, LOCKED = 4 };
#define MAX_SECRET (1024 * 1024)

// Only Kybern's own services, like `valid_service` in the CLI: this must not read any other app's items.
static int valid_service(const char *s) {
  const char *rest = NULL;
  if (!strncmp(s, "dev.kybern.", 11)) rest = s + 11;
  else if (!strncmp(s, "kybern-", 7)) rest = s + 7;
  if (!rest || !*rest) return 0;
  for (; *rest; rest++)
    if (!((*rest >= 'a' && *rest <= 'z') || (*rest >= 'A' && *rest <= 'Z') || (*rest >= '0' && *rest <= '9') || *rest == '.' || *rest == '-' || *rest == '_')) return 0;
  return 1;
}

static int fail(OSStatus status) {
  switch (status) {
  case errSecSuccess: return DONE;
  case errSecItemNotFound: fputs("No saved credential. Pair this environment again.\n", stderr); return NOT_FOUND;
  case errSecAuthFailed: case errSecInteractionNotAllowed: case errSecUserCanceled: case errSecNoDefaultKeychain: case -25315 /* errSecInteractionRequired */:
    fprintf(stderr, "The credential store is locked. Unlock your keyring, then try again. (Keychain %d)\n", (int)status); return LOCKED;
  default: fprintf(stderr, "The system credential store failed: Keychain %d\n", (int)status); return FAILED;
  }
}

int main(int argc, char **argv) {
  if (argc != 7 || strcmp(argv[1], "credentials") || strcmp(argv[3], "--service") || strcmp(argv[5], "--account")) {
    fputs("usage: kybern-keychain credentials get|set|delete --service <service> --account <account>\n", stderr);
    return USAGE;
  }
  const char *op = argv[2], *service = argv[4], *account = argv[6];
  if (!valid_service(service) || !*account) { fputs("Only Kybern's own credentials can be used here.\n", stderr); return USAGE; }
  UInt32 sl = (UInt32)strlen(service), al = (UInt32)strlen(account);
  SecKeychainItemRef item = NULL;
  UInt32 length = 0;
  void *data = NULL;
  OSStatus status;
  if (!strcmp(op, "get")) {
    status = SecKeychainFindGenericPassword(NULL, sl, service, al, account, &length, &data, &item);
    if (status) return fail(status);
    int written = fwrite(data, 1, length, stdout) == length && fflush(stdout) == 0;
    SecKeychainItemFreeContent(NULL, data);
    CFRelease(item);
    return written ? DONE : FAILED;
  }
  if (!strcmp(op, "set")) {
    char *secret = malloc(MAX_SECRET + 1);
    size_t size = secret ? fread(secret, 1, MAX_SECRET + 1, stdin) : 0;
    if (!secret || ferror(stdin) || size > MAX_SECRET) { fputs("The secret on standard input is too large or unreadable.\n", stderr); free(secret); return FAILED; }
    status = SecKeychainFindGenericPassword(NULL, sl, service, al, account, NULL, NULL, &item);
    if (status == errSecItemNotFound) status = SecKeychainAddGenericPassword(NULL, sl, service, al, account, (UInt32)size, secret, NULL);
    else if (status == errSecSuccess) { status = SecKeychainItemModifyAttributesAndData(item, NULL, (UInt32)size, secret); CFRelease(item); }
    memset(secret, 0, size);
    free(secret);
    return fail(status);
  }
  if (!strcmp(op, "delete")) {
    status = SecKeychainFindGenericPassword(NULL, sl, service, al, account, NULL, NULL, &item);
    if (status) return fail(status);
    status = SecKeychainItemDelete(item);
    CFRelease(item);
    return fail(status);
  }
  fputs("usage: kybern-keychain credentials get|set|delete --service <service> --account <account>\n", stderr);
  return USAGE;
}
