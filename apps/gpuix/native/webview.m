#import <Cocoa/Cocoa.h>
#import <WebKit/WebKit.h>
#import <QuartzCore/QuartzCore.h>
#import <Security/Security.h>
#include <CommonCrypto/CommonDigest.h>

// This library never creates NSApplication. All AppKit/WebKit operations run on
// GPUI's main queue; Bun receives queued events through a threadsafe wakeup.
static void (*signalJS)(void);
static NSMutableArray<NSString *> *events;
static NSString *lastEvent;
static NSMutableDictionary *hosts;
static NSMutableDictionary<NSNumber *, NSDictionary *> *occlusions;
static id keyMonitor, mouseMonitor;
static BOOL pointerCaptured;
static void emit(NSDictionary *event) {
  NSData *data = [NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
  if (!data || data.length > 65536) return;
  BOOL wake = NO;
  @synchronized(events) { if (events.count < 512) { wake = !events.count; [events addObject:[[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding]]; } }
  if (wake && signalJS) signalJS();
}
static BOOL releaseFocus(NSWindow *window);
@interface KybernWebClip : NSView
@property NSArray<NSBezierPath *> *blocked;
@end
@implementation KybernWebClip
- (BOOL)isFlipped { return YES; }
- (NSView *)hitTest:(NSPoint)point {
  if (pointerCaptured) return nil;
  NSPoint local = [self convertPoint:point fromView:self.superview];
  CAShapeLayer *mask = (CAShapeLayer *)self.layer.mask;
  if (mask.path && !CGPathContainsPoint(mask.path, NULL, NSPointToCGPoint(local), false)) return nil;
  for (NSBezierPath *path in self.blocked) if ([path containsPoint:local]) return nil;
  return [super hitTest:point];
}
// WebKit passes a key on to the web view's next responder only when the page left it unhandled: no preventDefault and
// no default action (a field, a modal dialog or fullscreen closing). An unhandled Escape arrives as cancelOperation:
// and gives the keyboard to GPUI. It ends here, so GPUI never sees it (it would close a menu or cancel a turn awaiting
// approval). Other plain keys end here silently, as in Safari, instead of NSBeep; ⌘ keys continue up the chain.
- (void)cancelOperation:(id)sender { releaseFocus(self.window); }
- (void)keyDown:(NSEvent *)event {
  if (event.modifierFlags & NSEventModifierFlagCommand) [super keyDown:event];
}
@end
@interface KybernWebShade : NSView
@end
@implementation KybernWebShade
- (NSView *)hitTest:(NSPoint)point { return nil; }
@end
// GPUI's key view. Our clips are its siblings under the content view, so a web view that holds first responder keeps
// every keystroke: GPUIView is not in its responder chain and never claims first responder on click (Zed sets it once,
// at window creation). Unhandled keys then end in NSBeep.
static NSView *gpuiView(NSWindow *window) {
  NSMutableArray<NSView *> *queue = [NSMutableArray arrayWithObject:window.contentView];
  while (queue.count) {
    NSView *view = queue.firstObject; [queue removeObjectAtIndex:0];
    if ([NSStringFromClass(view.class) isEqual:@"GPUIView"]) return view;
    if (![view isKindOfClass:KybernWebClip.class]) [queue addObjectsFromArray:view.subviews];
  }
  return window.contentView;
}
static WKWebView *focusedWeb(NSWindow *window);
// Hand the keyboard back to GPUI when one of our documents holds it. WebKit blurs the page as the view resigns.
static BOOL releaseFocus(NSWindow *window) {
  if (!window || !focusedWeb(window)) return NO;
  return [window makeFirstResponder:gpuiView(window)];
}
@interface KybernWebHost : NSObject <WKNavigationDelegate, WKScriptMessageHandler, WKURLSchemeHandler>
@property NSString *identifier;
@property NSString *category;
@property WKWebView *web;
@property KybernWebClip *clip;
@property NSArray<NSArray<NSNumber *> *> *roundedClips;
@property BOOL placed;
@property NSNumber *generation;
@property NSArray<NSString *> *allowedOrigins;
@property NSInteger layer;
@property NSRect logicalClip;
@property NSView *shade;
@property WKFrameInfo *childFrame;
@property NSURL *wrapperURL;
@property NSString *wrapperHTML;
@property BOOL lastVisible;
@property BOOL hasVisibility;
/// SHA-256 fingerprints (lowercase hex of the DER) of the certificates this document accepts for its `https://localhost`
/// origin: the per-install certificate of a port forward's Fast (HTTP/2) listener. Nothing else is trusted this way.
@property NSArray<NSString *> *trustedCertificates;
@end
static BOOL isLoopbackHost(NSString *host) {
  host = host.lowercaseString;
  return [host isEqual:@"localhost"] || [host isEqual:@"127.0.0.1"] || [host isEqual:@"::1"] || [host isEqual:@"[::1]"];
}
static NSString *leafFingerprint(SecTrustRef trust) {
  CFArrayRef chain = SecTrustCopyCertificateChain(trust);
  if (!chain) return nil;
  NSString *hex = nil;
  if (CFArrayGetCount(chain) > 0) {
    SecCertificateRef leaf = (SecCertificateRef)CFArrayGetValueAtIndex(chain, 0);
    CFDataRef der = SecCertificateCopyData(leaf);
    if (der) {
      unsigned char digest[CC_SHA256_DIGEST_LENGTH];
      CC_SHA256(CFDataGetBytePtr(der), (CC_LONG)CFDataGetLength(der), digest);
      NSMutableString *text = [NSMutableString stringWithCapacity:CC_SHA256_DIGEST_LENGTH * 2];
      for (int i = 0; i < CC_SHA256_DIGEST_LENGTH; i++) [text appendFormat:@"%02x", digest[i]];
      hex = text; CFRelease(der);
    }
  }
  CFRelease(chain);
  return hex;
}
@implementation KybernWebHost
// A Fast preview's `https://localhost` listener presents Kybern's own certificate, which no trust store knows: accept it
// only for a loopback host of an allowed origin and only when the leaf is exactly the pinned certificate.
- (void)webView:(WKWebView *)web didReceiveAuthenticationChallenge:(NSURLAuthenticationChallenge *)challenge completionHandler:(void (^)(NSURLSessionAuthChallengeDisposition, NSURLCredential *))done {
  NSURLProtectionSpace *space = challenge.protectionSpace;
  SecTrustRef trust = space.serverTrust;
  if ([space.authenticationMethod isEqual:NSURLAuthenticationMethodServerTrust] && trust && self.trustedCertificates.count && isLoopbackHost(space.host)) {
    NSURLComponents *origin = [NSURLComponents new]; origin.scheme = @"https"; origin.host = space.host; origin.port = @(space.port); origin.path = @"/";
    NSString *fingerprint = leafFingerprint(trust);
    if (fingerprint && [self.trustedCertificates containsObject:fingerprint] && origin.URL && [self allowsURL:origin.URL]) {
      done(NSURLSessionAuthChallengeUseCredential, [NSURLCredential credentialForTrust:trust]);
      return;
    }
  }
  done(NSURLSessionAuthChallengePerformDefaultHandling, nil);
}
- (void)webView:(WKWebView *)web startURLSchemeTask:(id<WKURLSchemeTask>)task {
  if (![task.request.URL isEqual:self.wrapperURL]) { [task didFailWithError:[NSError errorWithDomain:NSURLErrorDomain code:NSURLErrorResourceUnavailable userInfo:nil]]; return; }
  NSData *html = [self.wrapperHTML dataUsingEncoding:NSUTF8StringEncoding];
  [task didReceiveResponse:[[NSURLResponse alloc] initWithURL:self.wrapperURL MIMEType:@"text/html" expectedContentLength:html.length textEncodingName:@"utf-8"]];
  [task didReceiveData:html]; [task didFinish];
}
- (void)webView:(WKWebView *)web stopURLSchemeTask:(id<WKURLSchemeTask>)task {}
- (BOOL)allowsURL:(NSURL *)url {
  NSURLComponents *parts = [NSURLComponents componentsWithURL:url resolvingAgainstBaseURL:NO];
  if (![parts.scheme isEqual:@"http"] && ![parts.scheme isEqual:@"https"]) return NO;
  NSString *host = parts.host; if ([host containsString:@":"] && ![host hasPrefix:@"["]) host = [NSString stringWithFormat:@"[%@]",host];
  BOOL defaultPort = !parts.port || ([parts.scheme isEqual:@"http"] && parts.port.integerValue==80) || ([parts.scheme isEqual:@"https"] && parts.port.integerValue==443);
  NSString *origin = [NSString stringWithFormat:@"%@://%@%@", parts.scheme.lowercaseString, host.lowercaseString, defaultPort ? @"" : [@":" stringByAppendingString:parts.port.stringValue]];
  return !parts.user.length && !parts.password.length && [self.allowedOrigins containsObject:origin];
}
- (void)userContentController:(WKUserContentController *)controller didReceiveScriptMessage:(WKScriptMessage *)message {
  if ([message.name isEqual:@"kybernFrame"] && !message.frameInfo.isMainFrame) {
    self.childFrame = message.frameInfo;
    // A dictionary comes from the picker (`src/pick/pickerScript.ts`, run in this isolated world): the page's own scripts
    // cannot reach this handler. Its fields are page data and stay untrusted, so the client bounds them again.
    if ([message.body isKindOfClass:NSDictionary.class]) { emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"frameMessage",@"data":message.body}); return; }
    emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"navigation",@"url":message.frameInfo.request.URL.absoluteString ?: @""}); return;
  }
  // The untrusted child has the same handler name but cannot send host events.
  if (!message.frameInfo.isMainFrame || ![message.body isKindOfClass:NSDictionary.class]) return;
  emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"message",@"data":message.body});
}
- (void)webView:(WKWebView *)web decidePolicyForNavigationAction:(WKNavigationAction *)action decisionHandler:(void (^)(WKNavigationActionPolicy))done {
  // Only the wrapper may be top-level. Child navigation is additionally
  // constrained by iframe sandbox + daemon CSP. No popups, downloads or schemes.
  NSString *scheme = action.request.URL.scheme.lowercaseString;
  BOOL allowed = [scheme isEqual:@"http"] || [scheme isEqual:@"https"] || [scheme isEqual:@"about"];
  if (allowed && ![scheme isEqual:@"about"]) allowed = [self allowsURL:action.request.URL];
  if (action.targetFrame.isMainFrame) allowed = [action.request.URL isEqual:self.wrapperURL];
  if (!action.targetFrame) allowed = NO;
  if (!allowed) emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"externalNavigation",@"url":action.request.URL.absoluteString ?: @""});
  done(allowed ? WKNavigationActionPolicyAllow : WKNavigationActionPolicyCancel);
}
- (void)webView:(WKWebView *)web decidePolicyForNavigationResponse:(WKNavigationResponse *)response decisionHandler:(void (^)(WKNavigationResponsePolicy))done {
  NSURL *url = response.response.URL;
  BOOL allowed = [url isEqual:self.wrapperURL] || [url.scheme isEqual:@"about"] || (!response.isForMainFrame && [self allowsURL:url]);
  if (!allowed) emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"externalNavigation",@"url":url.absoluteString ?: @""});
  done(allowed ? WKNavigationResponsePolicyAllow : WKNavigationResponsePolicyCancel);
}
- (void)webViewWebContentProcessDidTerminate:(WKWebView *)web { emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"terminated"}); }
- (void)webView:(WKWebView *)web didFinishNavigation:(WKNavigation *)navigation { self.hasVisibility = NO; [self visibility]; emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"loaded"}); }
- (void)webView:(WKWebView *)web didFailProvisionalNavigation:(WKNavigation *)navigation withError:(NSError *)error {
  if (error.code != NSURLErrorCancelled) emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"failed",@"message":error.localizedDescription});
}
- (void)visibility {
  BOOL visible = self.placed && self.clip.window.isVisible && !self.clip.window.isMiniaturized && (self.clip.window.occlusionState & NSWindowOcclusionStateVisible);
  // Before hiding: AppKit resigns a hidden first responder to the window itself, which handles no keys.
  if (!visible && focusedWeb(self.web.window) == self.web) [self.web.window makeFirstResponder:gpuiView(self.web.window)];
  self.clip.hidden = !visible;
  if (self.hasVisibility && self.lastVisible == visible) return;
  self.hasVisibility = YES; self.lastVisible = visible;
  emit(@{@"id":self.identifier,@"generation":self.generation,@"type":@"visibility",@"visible":@(visible)});
  [self.web setAllMediaPlaybackSuspended:!visible completionHandler:^{}];
  NSString *script = [NSString stringWithFormat:@"window.kybernVisible && window.kybernVisible(%@)", visible ? @"true" : @"false"];
  [self.web evaluateJavaScript:script completionHandler:nil];
}
- (void)mask {
  CGFloat ownRadius = self.clip.layer.cornerRadius;
  CGPathRef path = CGPathCreateWithRoundedRect(NSRectToCGRect(self.clip.bounds), ownRadius, ownRadius, NULL);
  for (NSArray<NSNumber *> *shape in self.roundedClips) {
    if (shape.count != 8) continue;
    CGFloat x = shape[0].doubleValue - self.logicalClip.origin.x, y = shape[1].doubleValue - self.logicalClip.origin.y;
    CGFloat w = shape[2].doubleValue, h = shape[3].doubleValue;
    CGFloat tl = shape[4].doubleValue, tr = shape[5].doubleValue, br = shape[6].doubleValue, bl = shape[7].doubleValue;
    CGMutablePathRef rounded = CGPathCreateMutable();
    CGPathMoveToPoint(rounded, NULL, x+tl, y);
    CGPathAddLineToPoint(rounded, NULL, x+w-tr, y);
    CGPathAddArcToPoint(rounded, NULL, x+w, y, x+w, y+tr, tr);
    CGPathAddLineToPoint(rounded, NULL, x+w, y+h-br);
    CGPathAddArcToPoint(rounded, NULL, x+w, y+h, x+w-br, y+h, br);
    CGPathAddLineToPoint(rounded, NULL, x+bl, y+h);
    CGPathAddArcToPoint(rounded, NULL, x, y+h, x, y+h-bl, bl);
    CGPathAddLineToPoint(rounded, NULL, x, y+tl);
    CGPathAddArcToPoint(rounded, NULL, x, y, x+tl, y, tl);
    CGPathCloseSubpath(rounded);
    CGPathRef next = CGPathCreateCopyByIntersectingPath(path, rounded, false);
    CGPathRelease(rounded); CGPathRelease(path); path = next;
  }
  NSMutableArray<NSBezierPath *> *blocked = [NSMutableArray new]; CGFloat dim = 0;
  for (NSNumber *layer in occlusions) {
    NSDictionary *entry = occlusions[layer], *r = entry[@"rect"];
    if ([entry[@"order"] integerValue] <= self.layer) continue;
    NSRect hole = NSMakeRect([r[@"x"] doubleValue] - self.logicalClip.origin.x, [r[@"y"] doubleValue] - self.logicalClip.origin.y, [r[@"width"] doubleValue], [r[@"height"] doubleValue]);
    CGFloat radius = [entry[@"radius"] doubleValue];
    [blocked addObject:[NSBezierPath bezierPathWithRoundedRect:hole xRadius:radius yRadius:radius]];
    if ([entry[@"inputOnly"] boolValue]) continue;
    if ([entry[@"dim"] doubleValue] > 0) { if (NSIntersectsRect(self.clip.bounds,hole)) dim = MAX(dim,[entry[@"dim"] doubleValue]); continue; }
    CGPathRef cut = CGPathCreateWithRoundedRect(NSRectToCGRect(hole), radius, radius, NULL);
    CGPathRef next = CGPathCreateCopyBySubtractingPath(path,cut,false); CGPathRelease(cut); CGPathRelease(path); path = next;
  }
  self.clip.blocked = blocked;
  CAShapeLayer *mask = [CAShapeLayer layer]; mask.path = path; self.clip.layer.mask = mask; CGPathRelease(path);
  self.shade.frame = self.clip.bounds; self.shade.layer.backgroundColor = [NSColor colorWithWhite:0 alpha:dim].CGColor;
}
- (void)dispose {
  [NSNotificationCenter.defaultCenter removeObserver:self];
  if (focusedWeb(self.web.window) == self.web) [self.web.window makeFirstResponder:gpuiView(self.web.window)];
  [self.web stopLoading]; self.web.navigationDelegate = nil;
  [self.web.configuration.userContentController removeScriptMessageHandlerForName:@"kybern"];
  [self.web.configuration.userContentController removeScriptMessageHandlerForName:@"kybernFrame" contentWorld:[WKContentWorld worldWithName:@"KybernPreview"]];
  [self.clip removeFromSuperview]; self.web = nil; self.clip = nil;
}
@end

static WKWebView *focusedWeb(NSWindow *window) {
  NSResponder *responder = window.firstResponder;
  if (![responder isKindOfClass:NSView.class]) return nil;
  for (KybernWebHost *host in hosts.allValues) if (host.web && [(NSView *)responder isDescendantOf:host.web]) return host.web;
  return nil;
}
void kybern_web_init(void (*callback)(void)) { events = [NSMutableArray new]; hosts = [NSMutableDictionary new]; occlusions = [NSMutableDictionary new]; signalJS = callback; }
const char *kybern_web_next(void) {
  @synchronized(events) { if (!events.count) return NULL; lastEvent = events.firstObject; [events removeObjectAtIndex:0]; return lastEvent.UTF8String; }
}
void kybern_web_command(const char *json) {
  NSDictionary *cmd = [NSJSONSerialization JSONObjectWithData:[[NSString stringWithUTF8String:json] dataUsingEncoding:NSUTF8StringEncoding] options:0 error:nil];
  if (!cmd) return;
  dispatch_async(dispatch_get_main_queue(), ^{
    NSString *identifier = cmd[@"id"], *op = cmd[@"op"];
    if ([op isEqual:@"capture"]) { pointerCaptured = [cmd[@"active"] boolValue]; return; }
    if ([op isEqual:@"release"]) { for (NSWindow *w in NSApp.windows) releaseFocus(w); return; }
    if ([op isEqual:@"occlusion"]) {
      if ([cmd[@"rect"] isKindOfClass:NSDictionary.class]) occlusions[cmd[@"layer"]] = cmd; else [occlusions removeObjectForKey:cmd[@"layer"]];
      for (KybernWebHost *host in hosts.allValues) [host mask]; return;
    }
    KybernWebHost *host = hosts[identifier];
    if ([op isEqual:@"create"]) {
      if (!keyMonitor) keyMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskKeyDown handler:^NSEvent *(NSEvent *event) {
        if (!(event.modifierFlags & NSEventModifierFlagCommand) || event.isARepeat) return event;
        NSString *key = event.charactersIgnoringModifiers.lowercaseString;
        if ([key isEqual:@"\uf700"]) key = @"up";
        BOOL alt = (event.modifierFlags & NSEventModifierFlagOption) != 0;
        if (!(alt ? [key isEqual:@"p"] : [@[@"l",@"r",@"n",@"b",@"k",@"j",@"[",@"]",@"up",@",",@"\\"] containsObject:key])) return event;
        for (KybernWebHost *h in hosts.allValues) {
          NSResponder *responder = event.window.firstResponder;
          if ([responder isKindOfClass:NSView.class] && [(NSView *)responder isDescendantOf:h.web]) {
            emit(@{@"id":h.identifier,@"type":@"shortcut",@"key":key,@"modifiers":@{@"cmd":@YES,@"ctrl":@((event.modifierFlags & NSEventModifierFlagControl)!=0),@"alt":@(alt),@"shift":@((event.modifierFlags & NSEventModifierFlagShift)!=0)}}); return nil;
          }
        }
        return event;
      }];
      // A press anywhere outside the document that holds the keyboard (GPUI, a popup cut out of the document, the
      // title bar) gives it back to GPUI before the press is dispatched. A press in another document moves it there.
      if (!mouseMonitor) mouseMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown | NSEventMaskOtherMouseDown handler:^NSEvent *(NSEvent *event) {
        NSWindow *window = event.window; WKWebView *web = focusedWeb(window);
        NSView *frame = window.contentView.superview ?: window.contentView;
        NSView *hit = [frame hitTest:[frame convertPoint:event.locationInWindow fromView:nil]];
        // A live inline visual goes back to its picture on a press anywhere else: tell its owner where the press landed
        // (window points, top-left origin) so a press on its own caption does not count.
        NSPoint at = event.locationInWindow; CGFloat top = window.contentView.bounds.size.height - at.y;
        for (KybernWebHost *h in hosts.allValues)
          if ([h.category isEqual:@"visual"] && h.placed && h.web.window == window && !(hit && [hit isDescendantOf:h.web]))
            emit(@{@"id":h.identifier,@"generation":h.generation,@"type":@"outsidePress",@"x":@(at.x),@"y":@(top)});
        if (!web) return event;
        for (KybernWebHost *h in hosts.allValues) if (hit && h.web && [hit isDescendantOf:h.web]) return event;
        releaseFocus(window); return event;
      }];
      NSUInteger count = 0; for (KybernWebHost *h in hosts.allValues) if ([h.category isEqual:@"preview"]) count++;
      if (host || ([cmd[@"category"] isEqual:@"preview"] && count >= 2)) { emit(@{@"id":identifier,@"type":@"failed",@"message":@"Close another preview and try again."}); return; }
      NSWindow *window = nil; for (NSWindow *w in NSApp.windows) if (w.canBecomeKeyWindow) { window = w; break; }
      if (!window) { emit(@{@"id":identifier,@"type":@"failed",@"message":@"The preview window is unavailable."}); return; }
      host = [KybernWebHost new]; host.identifier = identifier; host.category = cmd[@"category"]; host.generation = cmd[@"generation"];
      host.allowedOrigins = cmd[@"allowedOrigins"];
      host.trustedCertificates = [cmd[@"trustedCertificates"] isKindOfClass:NSArray.class] ? cmd[@"trustedCertificates"] : @[];
      host.wrapperHTML = cmd[@"html"]; host.wrapperURL = [NSURL URLWithString:[@"tauri://localhost/embedded/" stringByAppendingString:[identifier stringByAddingPercentEncodingWithAllowedCharacters:NSCharacterSet.URLPathAllowedCharacterSet]]];
      WKWebViewConfiguration *config = [WKWebViewConfiguration new]; config.websiteDataStore = WKWebsiteDataStore.nonPersistentDataStore;
      [config setURLSchemeHandler:host forURLScheme:@"tauri"];
      config.preferences.javaScriptCanOpenWindowsAutomatically = NO;
      [config.userContentController addScriptMessageHandler:host name:@"kybern"];
      WKContentWorld *world = [WKContentWorld worldWithName:@"KybernPreview"];
      [config.userContentController addScriptMessageHandler:host contentWorld:world name:@"kybernFrame"];
      NSString *frameScript = @"if(window.parent!==window&&window.parent===window.top){const send=()=>window.webkit.messageHandlers.kybernFrame.postMessage('frame');send();addEventListener('popstate',send);addEventListener('hashchange',send)}";
      [config.userContentController addUserScript:[[WKUserScript alloc] initWithSource:frameScript injectionTime:WKUserScriptInjectionTimeAtDocumentEnd forMainFrameOnly:NO inContentWorld:world]];
      host.clip = [KybernWebClip new]; host.clip.wantsLayer = YES; host.clip.layer.masksToBounds = YES; host.clip.hidden = YES;
      host.web = [[WKWebView alloc] initWithFrame:NSZeroRect configuration:config]; host.web.navigationDelegate = host;
      // A visual sits on the thread's own material, like its picture: nothing is painted behind the page.
      if ([host.category isEqual:@"visual"]) @try { [host.web setValue:@NO forKey:@"drawsBackground"]; } @catch (NSException *e) {}
      host.shade = [KybernWebShade new]; host.shade.wantsLayer = YES;
      [host.clip addSubview:host.web]; [host.clip addSubview:host.shade]; [window.contentView addSubview:host.clip]; hosts[identifier] = host;
      for (NSNotificationName name in @[NSWindowDidChangeOcclusionStateNotification, NSWindowDidMiniaturizeNotification, NSWindowDidDeminiaturizeNotification]) [NSNotificationCenter.defaultCenter addObserver:host selector:@selector(visibility) name:name object:window];
      [host.web loadRequest:[NSURLRequest requestWithURL:host.wrapperURL]];
      emit(@{@"id":identifier,@"generation":host.generation,@"type":@"created",@"windowNumber":@(window.windowNumber)}); return;
    }
    if (!host) return;
    if ([op isEqual:@"hide"]) { host.placed = NO; [host visibility]; return; }
    if ([op isEqual:@"navigate"]) {
      NSURL *url = [NSURL URLWithString:cmd[@"url"]];
      if (![host allowsURL:url]) return;
      NSData *literal = [NSJSONSerialization dataWithJSONObject:@[url.absoluteString] options:NSJSONWritingFragmentsAllowed error:nil];
      NSString *array = [[NSString alloc] initWithData:literal encoding:NSUTF8StringEncoding];
      NSString *script = [NSString stringWithFormat:host.childFrame ? @"{const url=(%@)[0];if(location.href===url)location.reload();else location.replace(url)}" : @"document.querySelector('iframe').src=(%@)[0]", array];
      // DOM is shared, JavaScript globals are not. Page code cannot replace this command.
      [host.web evaluateJavaScript:script inFrame:host.childFrame inContentWorld:[WKContentWorld worldWithName:@"KybernPreview"] completionHandler:^(id value, NSError *error) {}];
      return;
    }
    if ([op isEqual:@"action"]) {
      if ([cmd[@"action"] isEqual:@"stop"]) { [host.web stopLoading]; return; }
      NSDictionary *scripts = @{@"back":@"history.back()",@"forward":@"history.forward()",@"reload":@"location.reload()",@"stop":@"window.stop()"};
      NSString *script = scripts[cmd[@"action"]];
      if (script && host.childFrame) [host.web evaluateJavaScript:script inFrame:host.childFrame inContentWorld:[WKContentWorld worldWithName:@"KybernPreview"] completionHandler:^(id value, NSError *error) {}];
      return;
    }
    if ([op isEqual:@"destroy"]) { [host dispose]; [hosts removeObjectForKey:identifier]; if (!hosts.count && keyMonitor) { [NSEvent removeMonitor:keyMonitor]; keyMonitor = nil; } if (!hosts.count && mouseMonitor) { [NSEvent removeMonitor:mouseMonitor]; mouseMonitor = nil; } emit(@{@"id":identifier,@"generation":host.generation,@"type":@"destroyed"}); return; }
    if ([op isEqual:@"place"]) {
      NSView *content = host.clip.superview;
      double x = [cmd[@"x"] doubleValue], y = [cmd[@"y"] doubleValue], width = [cmd[@"width"] doubleValue], height = [cmd[@"height"] doubleValue];
      host.clip.frame = NSMakeRect(x, content.isFlipped ? y : content.bounds.size.height - y - height, width, height);
      host.clip.layer.cornerRadius = [cmd[@"radius"] doubleValue];
      host.roundedClips = [cmd[@"roundedClips"] isKindOfClass:NSArray.class] ? cmd[@"roundedClips"] : @[];
      host.logicalClip = NSMakeRect(x,y,width,height); host.layer = [cmd[@"layer"] integerValue]; [host mask];
      host.web.frame = NSMakeRect([cmd[@"offsetX"] doubleValue], [cmd[@"offsetY"] doubleValue], [cmd[@"logicalWidth"] doubleValue], [cmd[@"logicalHeight"] doubleValue]);
      // WKWebView magnification does not change the layout viewport. The wrapper
      // sizes the iframe and scales its rendering, matching Tauri device mode.
      NSString *layout = cmd[@"layout"];
      if (layout) [host.web evaluateJavaScript:layout completionHandler:nil];
      host.placed = [cmd[@"visible"] boolValue] && width > 0 && height > 0; [host visibility]; return;
    }
    if ([op isEqual:@"present"]) {
      // N3: the document fades and scales with its host's `paintTransform` (an entrance, the mini player's exit). Core
      // Animation runs the curve in WindowServer, so the app presents no frames for it. The clip is flipped: origin is
      // fractions of its box from the top-left. `fromOpacity`/`fromScale` start an entrance; otherwise it eases from
      // what is on screen.
      CALayer *layer = host.clip.layer;
      CGSize size = layer.bounds.size;
      CGFloat ox = [cmd[@"originX"] doubleValue] * size.width, oy = [cmd[@"originY"] doubleValue] * size.height;
      CATransform3D (^about)(double) = ^CATransform3D(double scale) {
        return CATransform3DConcat(CATransform3DConcat(CATransform3DMakeTranslation(-ox, -oy, 0), CATransform3DMakeScale(scale, scale, 1)), CATransform3DMakeTranslation(ox, oy, 0));
      };
      CALayer *shown = layer.presentationLayer ?: layer;
      double opacity = [cmd[@"opacity"] doubleValue], scale = [cmd[@"scale"] doubleValue], duration = [cmd[@"duration"] doubleValue];
      NSNumber *fromOpacity = cmd[@"fromOpacity"], *fromScale = cmd[@"fromScale"];
      NSArray *ease = [cmd[@"ease"] isKindOfClass:NSArray.class] && [cmd[@"ease"] count] == 4 ? cmd[@"ease"] : @[@0.23, @1, @0.32, @1];
      [CATransaction begin]; [CATransaction setDisableActions:YES];
      if (duration > 0) {
        CAMediaTimingFunction *curve = [CAMediaTimingFunction functionWithControlPoints:[ease[0] floatValue] :[ease[1] floatValue] :[ease[2] floatValue] :[ease[3] floatValue]];
        CABasicAnimation *fade = [CABasicAnimation animationWithKeyPath:@"opacity"];
        fade.fromValue = fromOpacity ?: @(shown.opacity); fade.toValue = @(opacity); fade.duration = duration; fade.timingFunction = curve;
        CABasicAnimation *grow = [CABasicAnimation animationWithKeyPath:@"transform"];
        grow.fromValue = [NSValue valueWithCATransform3D:fromScale ? about(fromScale.doubleValue) : shown.transform]; grow.toValue = [NSValue valueWithCATransform3D:about(scale)];
        grow.duration = duration; grow.timingFunction = curve;
        [layer addAnimation:fade forKey:@"kybern-present-opacity"]; [layer addAnimation:grow forKey:@"kybern-present-transform"];
      } else {
        [layer removeAnimationForKey:@"kybern-present-opacity"]; [layer removeAnimationForKey:@"kybern-present-transform"];
      }
      layer.opacity = opacity; layer.transform = about(scale);
      [CATransaction commit]; return;
    }
    if ([op isEqual:@"probe"]) {
      CALayer *shown = host.clip.layer.presentationLayer ?: host.clip.layer;
      emit(@{@"id":identifier,@"type":@"probe",@"data":@{@"opacity":@(shown.opacity),@"scale":@(shown.transform.m11),@"animating":@([host.clip.layer animationForKey:@"kybern-present-opacity"] != nil || [host.clip.layer animationForKey:@"kybern-present-transform"] != nil)}}); return;
    }
    if ([op isEqual:@"post"]) { [host.web evaluateJavaScript:cmd[@"script"] completionHandler:nil]; return; }
    if ([op isEqual:@"snapshot"]) {
      // The document as it looks now, written as a PNG at the backing scale (a live visual going back to its picture).
      NSString *path = cmd[@"path"], *identifier = host.identifier; NSNumber *generation = host.generation, *token = cmd[@"token"] ?: @0;
      WKSnapshotConfiguration *config = [WKSnapshotConfiguration new]; config.afterScreenUpdates = NO;
      [host.web takeSnapshotWithConfiguration:config completionHandler:^(NSImage *image, NSError *error) {
        CGImageRef cg = [image CGImageForProposedRect:NULL context:nil hints:nil];
        NSData *png = cg ? [[[NSBitmapImageRep alloc] initWithCGImage:cg] representationUsingType:NSBitmapImageFileTypePNG properties:@{}] : nil;
        BOOL written = png && [png writeToFile:path atomically:YES];
        emit(@{@"id":identifier,@"generation":generation,@"type":@"snapshot",@"token":token,@"ok":@(written),@"path":written ? path : @"",
               @"width":@(cg ? CGImageGetWidth(cg) : 0),@"height":@(cg ? CGImageGetHeight(cg) : 0),@"message":error.localizedDescription ?: @""});
      }];
      return;
    }
    if ([op isEqual:@"frame"]) {
      // Runs `script` in the page's frame, in the isolated `KybernPreview` world: the DOM is shared, JavaScript globals are
      // not, so the page cannot call or replace what runs here. The result (when `rid` is set) comes back as `frameResult`.
      NSString *script = cmd[@"script"], *rid = cmd[@"rid"];
      void (^fail)(NSString *) = ^(NSString *message) { if (rid) emit(@{@"id":identifier,@"generation":host.generation,@"type":@"frameResult",@"rid":rid,@"ok":@NO,@"message":message}); };
      if (![script isKindOfClass:NSString.class] || !host.childFrame) { fail(@"The page has not loaded yet."); return; }
      [host.web evaluateJavaScript:script inFrame:host.childFrame inContentWorld:[WKContentWorld worldWithName:@"KybernPreview"] completionHandler:^(id value, NSError *error) {
        if (!rid) return;
        if (error) { fail(error.localizedDescription ?: @"The page did not run the script."); return; }
        id result = value && [NSJSONSerialization isValidJSONObject:@[value]] ? value : [NSNull null];
        emit(@{@"id":identifier,@"generation":host.generation,@"type":@"frameResult",@"rid":rid,@"ok":@YES,@"value":result});
      }];
      return;
    }
    if ([op isEqual:@"snapshotRect"]) {
      // A PNG of a rectangle of the page (CSS pixels in the child frame's viewport) written to `path` under the temp folder.
      // The wrapper scales and offsets the child (device mode), so its box is read first.
      NSString *rid = cmd[@"rid"], *path = cmd[@"path"];
      void (^fail)(NSString *) = ^(NSString *message) { if (rid) emit(@{@"id":identifier,@"generation":host.generation,@"type":@"snapshotRect",@"rid":rid,@"ok":@NO,@"message":message}); };
      if (![path isKindOfClass:NSString.class] || ![path hasPrefix:NSTemporaryDirectory()] || ![path.pathExtension isEqual:@"png"]) { fail(@"The snapshot path is not allowed."); return; }
      double rx = [cmd[@"x"] doubleValue], ry = [cmd[@"y"] doubleValue], rw = [cmd[@"width"] doubleValue], rh = [cmd[@"height"] doubleValue];
      NSString *probe = @"(()=>{const f=document.querySelector('iframe');if(!f)return null;const r=f.getBoundingClientRect();return [r.left,r.top,r.width/(f.offsetWidth||r.width||1)]})()";
      [host.web evaluateJavaScript:probe completionHandler:^(id box, NSError *error) {
        NSArray *frame = [box isKindOfClass:NSArray.class] && [box count] == 3 ? box : @[@0, @0, @1];
        double scale = [frame[2] doubleValue] > 0 ? [frame[2] doubleValue] : 1;
        NSRect wanted = NSMakeRect([frame[0] doubleValue] + rx * scale, [frame[1] doubleValue] + ry * scale, MAX(1, rw * scale), MAX(1, rh * scale));
        wanted = NSIntersectionRect(wanted, host.web.bounds);
        if (NSIsEmptyRect(wanted)) { fail(@"The element is not on screen."); return; }
        WKSnapshotConfiguration *config = [WKSnapshotConfiguration new];
        config.rect = wanted; config.snapshotWidth = @(MIN(1200, wanted.size.width * 2));
        [host.web takeSnapshotWithConfiguration:config completionHandler:^(NSImage *image, NSError *snapError) {
          if (!image) { fail(snapError.localizedDescription ?: @"The page could not be captured."); return; }
          NSBitmapImageRep *rep = [[NSBitmapImageRep alloc] initWithData:image.TIFFRepresentation];
          NSData *png = [rep representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
          if (!png || ![png writeToFile:path atomically:YES]) { fail(@"The capture could not be saved."); return; }
          emit(@{@"id":identifier,@"generation":host.generation,@"type":@"snapshotRect",@"rid":rid ?: @"",@"ok":@YES,@"path":path,@"width":@(wanted.size.width),@"height":@(wanted.size.height)});
        }];
      }];
      return;
    }
    if ([op isEqual:@"focus"]) [host.clip.window makeFirstResponder:host.web];
  });
}
