import java.io.BufferedReader;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.RandomAccessFile;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URLDecoder;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

// FavInbox has two jobs.
//
// 1. Inbox: it saves the original of every incoming private-chat image into a
//    directory the host can read (redroid bind-mounts /data, so external storage
//    is on disk). Each item is <msgId>.<ext> plus <msgId>.json; the JSON is
//    written last and marks the item complete. Long-pressing a conversation
//    offers a backfill of its older images. The fav `wechat` source drains this
//    through the host's favinbox_server.py.
//
// 2. Bridge: an HTTP API inside the WeChat process that exposes WAuxiliary's
//    capabilities (message events, history, original images, contacts, groups,
//    sending, Moments, and opening a message by tapping it) so fav decides what
//    to do with them. It listens on
//    BRIDGE_PORT inside the container and requires the token the plugin keeps in
//    FavBridge/token; the host's favinbox_server.py reads that file and forwards
//    /wx/* to it, so fav only ever talks to the host service. See README.md.
//
// The script runs in WAuxiliary's BeanShell, which is why it avoids generics and
// `var`, never returns from inside a try that has a finally (BeanShell then
// returns void), and keeps state shared with callbacks in Atomic* holders: script
// variables captured by a lambda or an anonymous class are treated as final.

String PLUGIN_VERSION = "0.4.3";
int WAIT_MS = 60000;
int PAGE_SIZE = 100;
int MAX_JSON_BODY = 1024 * 1024;
long MAX_UPLOAD = 200L * 1024 * 1024;
long EVENTS_ROTATE_BYTES = 16L * 1024 * 1024;
long UPLOAD_TTL_MS = 24L * 3600 * 1000;
int SEND_CALLBACK_MS = 15000;

String inboxDir = null;
String bridgeDir = null;
String uploadDir = null;
String bridgeToken = null;
long startedAt = System.currentTimeMillis();
java.util.concurrent.atomic.AtomicBoolean backfillRunning = new java.util.concurrent.atomic.AtomicBoolean(false);
java.util.concurrent.atomic.AtomicLong lastSeq = new java.util.concurrent.atomic.AtomicLong(0);
java.util.concurrent.atomic.AtomicLong firstSeqInCurrent = new java.util.concurrent.atomic.AtomicLong(0);
java.util.concurrent.locks.ReentrantLock eventLock = new java.util.concurrent.locks.ReentrantLock();
java.util.concurrent.atomic.AtomicReference serverRef = new java.util.concurrent.atomic.AtomicReference();
java.util.concurrent.ExecutorService pool = null;

void onLoad() {
    File base = hostContext.getExternalFilesDir(null);
    File dir = new File(base, "FavInbox");
    dir.mkdirs();
    inboxDir = dir.getAbsolutePath();
    File bdir = new File(base, "FavBridge");
    bdir.mkdirs();
    bridgeDir = bdir.getAbsolutePath();
    File udir = new File(cacheDir, "bridge-uploads");
    udir.mkdirs();
    uploadDir = udir.getAbsolutePath();
    bridgeToken = loadOrCreateToken();
    recoverSeq();
    log("FavInbox " + PLUGIN_VERSION + " loaded, inbox=" + inboxDir + ", bridge=" + bridgeDir + ", host=" + hostVerName);
    if (getBoolean("bridge_enabled", true)) {
        try {
            startBridge(getInt("bridge_port", 9971));
        } catch (Throwable e) {
            log("FavBridge failed to start: " + e);
        }
    }
}

void onUnload() {
    stopBridge();
}

// ---------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------

boolean alreadySaved(long msgId) {
    return new File(inboxDir, msgId + ".json").exists();
}

void onHandleMsg(Object msgInfoBean) {
    try {
        recordEvent("message", messageJson(msgInfoBean));
    } catch (Throwable e) {
        log("FavBridge event failed: " + e);
    }
    if (!getBoolean("inbox_enabled", true)) return;
    if (msgInfoBean.isSend()) return;
    if (msgInfoBean.isGroupChat() || msgInfoBean.isOfficialAccount()) return;
    if (inboxDir == null) return;

    final Object msg = msgInfoBean;
    final long msgId = msg.getMsgId();
    if (alreadySaved(msgId)) return;

    if (msg.isImage()) {
        new Thread(new Runnable() {
            public void run() {
                try {
                    saveImage(msg, msgId);
                } catch (Throwable e) {
                    log("FavInbox image " + msgId + " failed: " + e);
                    writeError(msg, msgId, "image", "error: " + e);
                }
            }
        }).start();
    } else if (msg.isVideo()) {
        writeMeta(msg, msgId, "video", null, "unsupported");
    } else if (msg.isFile()) {
        writeMeta(msg, msgId, "file", null, "unsupported");
    }
}

boolean saveImage(Object msg, long msgId) {
    File tmp = new File(inboxDir, msgId + ".part");
    String[] via = new String[1];
    File done = fetchOriginal(msg, tmp, via);
    if (done == null) {
        writeError(msg, msgId, "image", "download failed");
        return false;
    }
    File dest = new File(inboxDir, done.getName());
    done.renameTo(dest);
    log("FavInbox saved " + dest.getName() + " (" + dest.length() + " bytes, via " + via[0] + ")");
    writeMeta(msg, msgId, "image", dest, "ok:" + via[0]);
    return true;
}

// Downloads the original of an image message into `tmp`, strips WeChat's
// trailer and renames it to <msgId>.<ext> next to `tmp`. Returns that file, or
// null when neither the CDN download nor WeChat's local copy produced bytes.
File fetchOriginal(Object msg, File tmp, String[] via) {
    Object imageMsg = msg.getImageMsg();
    tmp.delete();
    via[0] = null;
    if (imageMsg != null) {
        downloadImg(imageMsg, tmp.getAbsolutePath());
        if (waitStable(tmp)) via[0] = "downloadImg";
    }
    if (via[0] == null) {
        // Fall back to whatever WeChat itself stored locally.
        String local = null;
        try { local = msg.getImgPath(); } catch (Throwable e) {}
        if (local != null && new File(local).isFile() && new File(local).length() > 0) {
            copyFile(new File(local), tmp);
            via[0] = "imgPath";
        }
    }
    if (via[0] == null) {
        tmp.delete();
        return null;
    }
    return finishImage(tmp, String.valueOf(msg.getMsgId()));
}

File finishImage(File tmp, String name) {
    stripWeChatTrailer(tmp);
    File dest = new File(tmp.getParentFile(), name + "." + sniffExt(tmp));
    dest.delete();
    tmp.renameTo(dest);
    return dest;
}

boolean waitStable(File f) {
    long deadline = System.currentTimeMillis() + WAIT_MS;
    long last = -1;
    int stable = 0;
    while (System.currentTimeMillis() < deadline) {
        long len = f.isFile() ? f.length() : 0;
        if (len > 0 && len == last) {
            stable++;
            if (stable >= 3) return true;
        } else {
            stable = 0;
        }
        last = len;
        Thread.sleep(500);
    }
    return f.isFile() && f.length() > 0;
}

// WeChat appends 24 bytes to downloaded images: 8 bytes of its own data and
// then a 16-byte hash. Drop them when the hash is the MD5 of the body, or when
// the body ends exactly on the image format's end marker.
void stripWeChatTrailer(File f) {
    long len = f.length();
    if (len <= 32) return;
    RandomAccessFile raf = new RandomAccessFile(f, "rw");
    try {
        byte[] head = new byte[2];
        raf.seek(0);
        raf.readFully(head);
        byte[] end = new byte[8];
        raf.seek(len - 32);
        raf.readFully(end);
        boolean jpegEnds = (head[0] & 0xff) == 0xff && (head[1] & 0xff) == 0xd8
            && (end[6] & 0xff) == 0xff && (end[7] & 0xff) == 0xd9;
        boolean pngEnds = (head[0] & 0xff) == 0x89 && end[0] == 'I' && end[1] == 'E' && end[2] == 'N' && end[3] == 'D';
        if (jpegEnds || pngEnds) {
            raf.setLength(len - 24);
            return;
        }
        byte[] tail = new byte[16];
        raf.seek(len - 16);
        raf.readFully(tail);
        MessageDigest md = MessageDigest.getInstance("MD5");
        byte[] buf = new byte[65536];
        long remaining = len - 24;
        raf.seek(0);
        while (remaining > 0) {
            int r = raf.read(buf, 0, (int) Math.min(buf.length, remaining));
            if (r <= 0) break;
            md.update(buf, 0, r);
            remaining -= r;
        }
        if (Arrays.equals(md.digest(), tail)) raf.setLength(len - 24);
    } finally {
        raf.close();
    }
}

String sniffExt(File f) {
    byte[] b = new byte[12];
    int n = 0;
    FileInputStream in = new FileInputStream(f);
    try { n = in.read(b); } finally { in.close(); }
    if (n >= 3 && (b[0] & 0xff) == 0xff && (b[1] & 0xff) == 0xd8) return "jpg";
    if (n >= 4 && (b[0] & 0xff) == 0x89 && b[1] == 'P' && b[2] == 'N' && b[3] == 'G') return "png";
    if (n >= 4 && b[0] == 'G' && b[1] == 'I' && b[2] == 'F') return "gif";
    if (n >= 12 && b[0] == 'R' && b[1] == 'I' && b[2] == 'F' && b[3] == 'F' && b[8] == 'W' && b[9] == 'E' && b[10] == 'B' && b[11] == 'P') return "webp";
    if (n >= 4 && b[0] == 'w' && b[1] == 'x' && b[2] == 'g' && b[3] == 'f') return "wxgf";
    return "bin";
}

void copyFile(File src, File dst) {
    FileInputStream in = new FileInputStream(src);
    FileOutputStream out = new FileOutputStream(dst);
    try {
        byte[] buf = new byte[65536];
        int r;
        while ((r = in.read(buf)) > 0) out.write(buf, 0, r);
    } finally {
        in.close();
        out.close();
    }
}

// Failures go to <msgId>.err so they are not picked up as finished items and a
// later backfill retries them.
void writeError(Object msg, long msgId, String type, String status) {
    writeJson(msg, msgId, type, null, status, msgId + ".err");
}

void writeMeta(Object msg, long msgId, String type, File file, String status) {
    writeJson(msg, msgId, type, file, status, msgId + ".json");
}

void writeJson(Object msg, long msgId, String type, File file, String status, String name) {
    JSONObject j = new JSONObject();
    j.put("msg_id", msgId);
    j.put("type", type);
    j.put("status", status);
    j.put("create_time", msg.getCreateTime());
    j.put("talker", msg.getTalker());
    j.put("sender", msg.getSendTalker());
    j.put("saved_at", System.currentTimeMillis());
    if (file != null) {
        j.put("file", file.getName());
        j.put("size", file.length());
    }
    if (type.equals("image") && msg.getImageMsg() != null) {
        j.put("md5", msg.getImageMsg().getMd5());
    }
    if (type.equals("file") && msg.getFileMsg() != null) {
        Object fm = msg.getFileMsg();
        j.put("title", fm.getTitle());
        j.put("ext", fm.getExt());
        j.put("declared_size", fm.getSize());
        j.put("md5", fm.getMd5());
    }
    writeFileAtomic(new File(inboxDir, name), j.toString().getBytes("UTF-8"));
}

void writeFileAtomic(File dest, byte[] data) {
    File tmp = new File(dest.getParentFile(), dest.getName() + ".part");
    FileOutputStream out = new FileOutputStream(tmp);
    try {
        out.write(data);
    } finally {
        out.close();
    }
    tmp.renameTo(dest);
}

void onCreateConversationItemMenu(Object conversationBean) {
    addConversationItemMenuItem("FavInbox 补抓图片", conversation -> {
        startBackfill(conversation.getUsername());
    });
}

void startBackfill(String talker) {
    if (inboxDir == null || talker == null) return;
    if (!backfillRunning.compareAndSet(false, true)) {
        toast("补抓已在进行中");
        return;
    }
    final String t = talker;
    new Thread(new Runnable() {
        public void run() {
            int saved = 0;
            int skipped = 0;
            int failed = 0;
            try {
                toast("开始补抓图片");
                long start = 0L;
                java.util.HashSet seen = new java.util.HashSet();
                while (true) {
                    java.util.List page = queryHistoryMsg(t, start, true, PAGE_SIZE);
                    if (page == null || page.isEmpty()) break;
                    long lastTime = start;
                    for (int i = 0; i < page.size(); i++) {
                        Object m = page.get(i);
                        lastTime = Math.max(lastTime, m.getCreateTime());
                        if (m.isSend() || !m.isImage()) continue;
                        long id = m.getMsgId();
                        if (!seen.add(Long.valueOf(id))) continue;
                        if (alreadySaved(id)) {
                            skipped++;
                            continue;
                        }
                        try {
                            if (saveImage(m, id)) saved++;
                            else failed++;
                        } catch (Throwable e) {
                            failed++;
                            log("FavInbox backfill " + id + " failed: " + e);
                        }
                    }
                    if (page.size() < PAGE_SIZE || lastTime <= start) break;
                    start = lastTime;
                }
            } catch (Throwable e) {
                log("FavInbox backfill aborted: " + e);
            } finally {
                backfillRunning.set(false);
            }
            String summary = "补抓完成：新存 " + saved + "，已有 " + skipped + "，失败 " + failed;
            log("FavInbox " + summary);
            toast(summary);
        }
    }).start();
}

// ---------------------------------------------------------------------------
// Bridge: events
// ---------------------------------------------------------------------------

void onNewFriend(String wxid, String ticket, int scene, JSONObject info) {
    JSONObject j = new JSONObject();
    j.put("wxid", wxid);
    j.put("ticket", ticket);
    j.put("scene", scene);
    j.put("info", info);
    recordEvent("new_friend", j);
}

void onMemberChange(String type, String groupWxid, String userWxid, String userName) {
    JSONObject j = new JSONObject();
    j.put("change", type);
    j.put("room", groupWxid);
    j.put("wxid", userWxid);
    j.put("name", userName);
    recordEvent("member_change", j);
}

void onRecvPayMsg(Object payMsgBean) {
    recordEvent("pay", beanJson(payMsgBean, 0));
}

// Events are appended to FavBridge/events.jsonl, one JSON object per line that
// always starts with {"seq":N, so a reader can find a sequence number without
// parsing the line. The file rolls over to events.1.jsonl at EVENTS_ROTATE_BYTES.
File eventsFile() { return new File(bridgeDir, "events.jsonl"); }
File oldEventsFile() { return new File(bridgeDir, "events.1.jsonl"); }

void recordEvent(String kind, JSONObject data) {
    if (bridgeDir == null) return;
    eventLock.lock();
    try {
        File f = eventsFile();
        if (f.length() > EVENTS_ROTATE_BYTES) {
            File old = oldEventsFile();
            old.delete();
            f.renameTo(old);
            firstSeqInCurrent.set(0);
        }
        long seq = lastSeq.incrementAndGet();
        if (firstSeqInCurrent.get() == 0) firstSeqInCurrent.set(seq);
        JSONObject rest = new JSONObject();
        rest.put("kind", kind);
        rest.put("at", System.currentTimeMillis());
        rest.put("data", data);
        String line = "{\"seq\":" + seq + "," + rest.toString().substring(1) + "\n";
        FileOutputStream out = new FileOutputStream(f, true);
        try {
            out.write(line.getBytes("UTF-8"));
        } finally {
            out.close();
        }
    } finally {
        eventLock.unlock();
    }
}

long lineSeq(String line) {
    if (!line.startsWith("{\"seq\":")) return -1;
    int end = line.indexOf(',', 7);
    if (end < 0) return -1;
    try {
        return Long.parseLong(line.substring(7, end));
    } catch (Throwable e) {
        return -1;
    }
}

String lastLine(File f) {
    if (!f.isFile() || f.length() == 0) return null;
    String s = null;
    RandomAccessFile raf = new RandomAccessFile(f, "r");
    try {
        long len = raf.length();
        int n = (int) Math.min(len, 65536L);
        byte[] buf = new byte[n];
        raf.seek(len - n);
        raf.readFully(buf);
        s = new String(buf, "UTF-8").trim();
    } finally {
        raf.close();
    }
    int nl = s.lastIndexOf('\n');
    return nl >= 0 ? s.substring(nl + 1) : s;
}

String firstLine(File f) {
    if (!f.isFile() || f.length() == 0) return null;
    String line = null;
    BufferedReader r = new BufferedReader(new InputStreamReader(new FileInputStream(f), "UTF-8"));
    try {
        line = r.readLine();
    } finally {
        r.close();
    }
    return line;
}

void recoverSeq() {
    long seq = 0;
    String last = lastLine(eventsFile());
    if (last == null) last = lastLine(oldEventsFile());
    if (last != null) seq = Math.max(0, lineSeq(last));
    lastSeq.set(seq);
    String first = firstLine(eventsFile());
    firstSeqInCurrent.set(first == null ? 0 : Math.max(0, lineSeq(first)));
}

// Up to `limit` raw event lines with seq > after, oldest first. Waits up to
// waitMs for the first new event, so a caller can long-poll.
List readEvents(long after, int limit, long waitMs) {
    long deadline = System.currentTimeMillis() + waitMs;
    while (lastSeq.get() <= after && System.currentTimeMillis() < deadline) {
        Thread.sleep(250);
    }
    List out = new ArrayList();
    if (lastSeq.get() <= after) return out;
    long first = firstSeqInCurrent.get();
    if (first == 0 || after + 1 < first) collectEvents(oldEventsFile(), after, limit, out);
    if (out.size() < limit) collectEvents(eventsFile(), after, limit, out);
    return out;
}

void collectEvents(File f, long after, int limit, List out) {
    if (!f.isFile()) return;
    BufferedReader r = new BufferedReader(new InputStreamReader(new FileInputStream(f), "UTF-8"));
    try {
        String line;
        while ((line = r.readLine()) != null && out.size() < limit) {
            if (lineSeq(line) > after) out.add(line);
        }
    } finally {
        r.close();
    }
}

// ---------------------------------------------------------------------------
// Bridge: JSON views of WAuxiliary objects
// ---------------------------------------------------------------------------

Object nn(Object v) {
    return v == null ? JSONObject.NULL : v;
}

Object getter(Object o, String name) {
    if (o == null) return null;
    try {
        return o.getClass().getMethod(name, new Class[0]).invoke(o, new Object[0]);
    } catch (Throwable e) {
        return null;
    }
}

boolean isTrue(Object o, String name) {
    Object v = getter(o, name);
    return v != null && v.equals(Boolean.TRUE);
}

boolean isSimple(Object v) {
    return v instanceof String || v instanceof Number || v instanceof Boolean || v instanceof Character;
}

String snake(String s) {
    StringBuilder b = new StringBuilder();
    for (int i = 0; i < s.length(); i++) {
        char c = s.charAt(i);
        if (Character.isUpperCase(c)) {
            if (i > 0) b.append('_');
            b.append(Character.toLowerCase(c));
        } else {
            b.append(c);
        }
    }
    return b.toString();
}

Object jsonValue(Object v, int depth) {
    if (v == null) return JSONObject.NULL;
    if (isSimple(v)) return v;
    if (v instanceof JSONObject || v instanceof JSONArray) return v;
    if (v instanceof java.util.Collection) {
        JSONArray a = new JSONArray();
        java.util.Iterator it = ((java.util.Collection) v).iterator();
        while (it.hasNext()) a.put(jsonValue(it.next(), depth));
        return a;
    }
    if (v instanceof Map) {
        JSONObject j = new JSONObject();
        java.util.Iterator it = ((Map) v).entrySet().iterator();
        while (it.hasNext()) {
            java.util.Map.Entry e = (java.util.Map.Entry) it.next();
            j.put(String.valueOf(e.getKey()), jsonValue(e.getValue(), depth));
        }
        return j;
    }
    if (depth <= 0) return String.valueOf(v);
    return beanJson(v, depth - 1);
}

// Every public no-argument getX()/isX() of a bean, keyed in snake_case. Used for
// the structures WAuxiliary does not document field by field (FriendInfo,
// GroupInfo, ContactLabelBean) and for the nested message structures.
JSONObject beanJson(Object o, int depth) {
    JSONObject j = new JSONObject();
    if (o == null) return j;
    java.lang.reflect.Method[] methods = o.getClass().getMethods();
    for (int i = 0; i < methods.length; i++) {
        java.lang.reflect.Method m = methods[i];
        if (m.getParameterTypes().length != 0) continue;
        if (m.getDeclaringClass() == Object.class) continue;
        String name = m.getName();
        String key = null;
        if (name.startsWith("get") && name.length() > 3) key = name.substring(3);
        else if (name.startsWith("is") && name.length() > 2) key = name.substring(2);
        if (key == null) continue;
        try {
            j.put(snake(key), jsonValue(m.invoke(o, new Object[0]), depth));
        } catch (Throwable e) {
            // A getter that throws is left out.
        }
    }
    return j;
}

String[] KIND_CHECKS = new String[] {
    "isRecalled", "recalled", "isSystem", "system", "isPat", "pat", "isQuote", "quote",
    "isImage", "image", "isVideo", "video", "isVoice", "voice", "isEmoji", "emoji",
    "isLocation", "location", "isShareCard", "share_card", "isLink", "link", "isFile", "file",
    "isVideoNumberVideo", "channel_video", "isNote", "note", "isTransfer", "transfer",
    "isRedBag", "red_bag", "isVoipVideo", "voip_video", "isVoipVoice", "voip_voice",
    "isVoip", "voip", "isApp", "app", "isText", "text"
};

String messageKind(Object msg) {
    for (int i = 0; i < KIND_CHECKS.length; i += 2) {
        if (isTrue(msg, KIND_CHECKS[i])) return KIND_CHECKS[i + 1];
    }
    return "other";
}

String chatKind(Object msg) {
    if (isTrue(msg, "isOfficialAccount")) return "official";
    if (isTrue(msg, "isImChatroom")) return "im_group";
    if (isTrue(msg, "isGroupChat") || isTrue(msg, "isChatroom")) return "group";
    if (isTrue(msg, "isOpenIM")) return "im_private";
    return "private";
}

JSONObject messageJson(Object msg) {
    JSONObject j = new JSONObject();
    j.put("msg_id", nn(getter(msg, "getMsgId")));
    j.put("type", nn(getter(msg, "getType")));
    j.put("kind", messageKind(msg));
    j.put("chat", chatKind(msg));
    j.put("create_time", nn(getter(msg, "getCreateTime")));
    j.put("talker", nn(getter(msg, "getTalker")));
    j.put("sender", nn(getter(msg, "getSendTalker")));
    j.put("is_send", isTrue(msg, "isSend"));
    j.put("is_at_me", isTrue(msg, "isAtMe"));
    j.put("is_notify_all", isTrue(msg, "isNotifyAll"));
    j.put("is_announce_all", isTrue(msg, "isAnnounceAll"));
    j.put("at_users", jsonValue(getter(msg, "getAtUserList"), 0));
    j.put("content", nn(getter(msg, "getContent")));
    j.put("msg_source", nn(getter(msg, "getMsgSource")));
    String[] nested = new String[] {"getImageMsg", "image", "getFileMsg", "file", "getQuoteMsg", "quote", "getPatMsg", "pat"};
    for (int i = 0; i < nested.length; i += 2) {
        Object v = getter(msg, nested[i]);
        if (v != null) j.put(nested[i + 1], beanJson(v, 0));
    }
    return j;
}

JSONArray messagesJson(List msgs) {
    JSONArray a = new JSONArray();
    if (msgs == null) return a;
    for (int i = 0; i < msgs.size(); i++) a.put(messageJson(msgs.get(i)));
    return a;
}

JSONArray beansJson(List items) {
    JSONArray a = new JSONArray();
    if (items == null) return a;
    for (int i = 0; i < items.size(); i++) {
        Object v = items.get(i);
        a.put(isSimple(v) ? v : beanJson(v, 0));
    }
    return a;
}

// ---------------------------------------------------------------------------
// Bridge: HTTP server
// ---------------------------------------------------------------------------

String loadOrCreateToken() {
    File f = new File(bridgeDir, "token");
    String existing = firstLine(f);
    if (existing != null && existing.trim().length() >= 32) return existing.trim();
    byte[] raw = new byte[32];
    new SecureRandom().nextBytes(raw);
    StringBuilder b = new StringBuilder();
    for (int i = 0; i < raw.length; i++) b.append(Integer.toHexString((raw[i] & 0xff) | 0x100).substring(1));
    writeFileAtomic(f, (b.toString() + "\n").getBytes("UTF-8"));
    log("FavBridge created a new token in " + f);
    return b.toString();
}

void startBridge(int port) {
    stopBridge();
    // A reload that skipped onUnload leaves the old socket bound; it is parked in
    // the system properties so this instance can close it.
    Object stale = System.getProperties().get("favbridge.server");
    if (stale != null) {
        try { ((ServerSocket) stale).close(); } catch (Throwable e) {}
    }
    final ServerSocket ss = new ServerSocket();
    ss.setReuseAddress(true);
    ss.bind(new InetSocketAddress("0.0.0.0", port));
    System.getProperties().put("favbridge.server", ss);
    serverRef.set(ss);
    pool = java.util.concurrent.Executors.newFixedThreadPool(8);
    final java.util.concurrent.ExecutorService workers = pool;
    Thread t = new Thread(new Runnable() {
        public void run() {
            while (!ss.isClosed()) {
                try {
                    final Socket s = ss.accept();
                    workers.execute(new Runnable() {
                        public void run() {
                            serve(s);
                        }
                    });
                } catch (Throwable e) {
                    if (!ss.isClosed()) log("FavBridge accept failed: " + e);
                }
            }
        }
    });
    t.setName("FavBridge");
    t.setDaemon(true);
    t.start();
    log("FavBridge listening on " + port);
}

void stopBridge() {
    Object ss = serverRef.getAndSet(null);
    if (ss != null) {
        try { ((ServerSocket) ss).close(); } catch (Throwable e) {}
        System.getProperties().remove("favbridge.server");
    }
    if (pool != null) {
        pool.shutdownNow();
        pool = null;
    }
}

// One request per connection: parse, dispatch, answer, close.
void serve(Socket s) {
    try {
        s.setSoTimeout(120000);
        InputStream in = new java.io.BufferedInputStream(s.getInputStream());
        OutputStream out = new java.io.BufferedOutputStream(s.getOutputStream());
        Map req = null;
        try {
            req = readRequest(in);
            // The body is read here rather than inside the route: under WAuxiliary
            // the route runs in a scripted Callable, which every POST used to leave
            // empty-handed. /upload streams its body, so it gets the stream instead.
            if (req != null) {
                String m = (String) req.get("method");
                if (req.get("path").equals("/upload")) req.put("stream", in);
                else if (m.equals("POST") || m.equals("PUT")) req.put("json", jsonBody(req, in));
            }
        } catch (Throwable e) {
            sendJson(out, 400, error("bad request: " + e.getMessage()));
            return;
        }
        if (req == null) return;
        Object result;
        int status = 200;
        try {
            result = guarded(req);
        } catch (Throwable e) {
            Throwable cause = rootCause(e);
            status = statusFor(cause);
            result = error(String.valueOf(cause.getMessage() == null ? cause.toString() : cause.getMessage()));
            if (status == 500) log("FavBridge " + req.get("method") + " " + req.get("path") + " failed: " + cause);
        }
        if (result == null) {
            // A route that came back empty, which WAuxiliary's BeanShell can do; say so
            // instead of failing on the answer and closing the connection unanswered.
            status = 500;
            result = error(req.get("method") + " " + req.get("path") + " returned nothing");
            log("FavBridge " + req.get("method") + " " + req.get("path") + " returned nothing");
        }
        if (result instanceof File) {
            sendFile(out, (File) result);
            ((File) result).delete();
        } else if (result instanceof JSONArray) {
            sendBytes(out, status, "application/json; charset=utf-8", result.toString().getBytes("UTF-8"));
        } else if (result instanceof String) {
            sendBytes(out, status, "application/json; charset=utf-8", ((String) result).getBytes("UTF-8"));
        } else {
            sendJson(out, status, (JSONObject) result);
        }
    } catch (Throwable e) {
        log("FavBridge connection failed: " + e);
    } finally {
        try { s.close(); } catch (Throwable e) {}
    }
}

// Runs a route behind a Java-level Callable, so a WAuxiliary function this host
// version lacks cannot kill the worker thread. The route's failure is caught
// inside the Callable and handed back as an ExecutionException: WAuxiliary's
// BeanShell can otherwise turn a script error there into a plain null.
Object guarded(Map req) {
    final Map r = req;
    java.util.concurrent.FutureTask task = new java.util.concurrent.FutureTask(new java.util.concurrent.Callable() {
        public Object call() {
            Object answer = null;
            try {
                answer = route(r);
            } catch (Throwable e) {
                answer = new java.util.concurrent.ExecutionException(e);
            }
            return answer;
        }
    });
    task.run();
    Object result = task.get();
    if (result instanceof java.util.concurrent.ExecutionException) throw (Throwable) result;
    return result;
}

Throwable rootCause(Throwable e) {
    Throwable c = e;
    for (int i = 0; i < 8; i++) {
        Throwable next = null;
        if (c instanceof java.lang.reflect.InvocationTargetException) next = ((java.lang.reflect.InvocationTargetException) c).getTargetException();
        else if (c instanceof java.util.concurrent.ExecutionException) next = c.getCause();
        else if (c instanceof java.lang.reflect.UndeclaredThrowableException) next = c.getCause();
        else if (c.getClass().getName().equals("bsh.TargetError")) next = (Throwable) getter(c, "getTarget");
        if (next == null || next == c) break;
        c = next;
    }
    return c;
}

// Exceptions double as HTTP statuses: SecurityException 401, FileNotFoundException
// 404, IllegalArgumentException/JSONException 400, anything else 500.
int statusFor(Throwable e) {
    if (e instanceof SecurityException) return 401;
    if (e instanceof java.io.FileNotFoundException) return 404;
    if (e instanceof IllegalArgumentException || e instanceof JSONException) return 400;
    return 500;
}

JSONObject error(String message) {
    JSONObject j = new JSONObject();
    j.put("ok", false);
    j.put("error", message);
    return j;
}

JSONObject ok() {
    JSONObject j = new JSONObject();
    j.put("ok", true);
    return j;
}

Map readRequest(InputStream in) {
    ByteArrayOutputStream head = new ByteArrayOutputStream();
    int matched = 0;
    while (matched < 4) {
        int c = in.read();
        if (c < 0) {
            if (head.size() == 0) return null;
            throw new IllegalArgumentException("truncated headers");
        }
        head.write(c);
        if (head.size() > 32768) throw new IllegalArgumentException("headers too large");
        if ((matched % 2 == 0 && c == '\r') || (matched % 2 == 1 && c == '\n')) matched++;
        else matched = c == '\r' ? 1 : 0;
    }
    String[] lines = new String(head.toByteArray(), "ISO-8859-1").split("\r\n");
    String[] first = lines[0].split(" ");
    if (first.length < 2) throw new IllegalArgumentException("bad request line");
    Map headers = new HashMap();
    for (int i = 1; i < lines.length; i++) {
        int colon = lines[i].indexOf(':');
        if (colon > 0) headers.put(lines[i].substring(0, colon).trim().toLowerCase(), lines[i].substring(colon + 1).trim());
    }
    String target = first[1];
    int q = target.indexOf('?');
    Map query = new HashMap();
    if (q >= 0) {
        String[] pairs = target.substring(q + 1).split("&");
        for (int i = 0; i < pairs.length; i++) {
            if (pairs[i].length() == 0) continue;
            int eq = pairs[i].indexOf('=');
            String k = eq >= 0 ? pairs[i].substring(0, eq) : pairs[i];
            String v = eq >= 0 ? pairs[i].substring(eq + 1) : "";
            query.put(URLDecoder.decode(k, "UTF-8"), URLDecoder.decode(v, "UTF-8"));
        }
    }
    Map req = new HashMap();
    req.put("method", first[0].toUpperCase());
    req.put("path", URLDecoder.decode(q >= 0 ? target.substring(0, q) : target, "UTF-8"));
    req.put("query", query);
    req.put("headers", headers);
    String cl = (String) headers.get("content-length");
    req.put("length", Long.valueOf(cl == null ? 0L : Long.parseLong(cl)));
    return req;
}

byte[] readBody(Map req, InputStream in, long max) {
    long len = ((Long) req.get("length")).longValue();
    if (len > max) throw new IllegalArgumentException("body too large");
    byte[] buf = new byte[(int) len];
    int off = 0;
    while (off < len) {
        int r = in.read(buf, off, (int) len - off);
        if (r < 0) throw new IllegalArgumentException("truncated body");
        off += r;
    }
    return buf;
}

JSONObject jsonBody(Map req, InputStream in) {
    byte[] body = readBody(req, in, MAX_JSON_BODY);
    if (body.length == 0) return new JSONObject();
    return new JSONObject(new String(body, "UTF-8"));
}

void sendJson(OutputStream out, int status, JSONObject j) {
    sendBytes(out, status, "application/json; charset=utf-8", j.toString().getBytes("UTF-8"));
}

String reason(int status) {
    if (status == 200) return "OK";
    if (status == 400) return "Bad Request";
    if (status == 401) return "Unauthorized";
    if (status == 404) return "Not Found";
    return "Internal Server Error";
}

void sendHead(OutputStream out, int status, String type, long length) {
    String head = "HTTP/1.1 " + status + " " + reason(status) + "\r\nContent-Type: " + type
        + "\r\nContent-Length: " + length + "\r\nConnection: close\r\n\r\n";
    out.write(head.getBytes("ISO-8859-1"));
}

void sendBytes(OutputStream out, int status, String type, byte[] body) {
    sendHead(out, status, type, body.length);
    out.write(body);
    out.flush();
}

String contentType(String name) {
    String n = name.toLowerCase();
    if (n.endsWith(".jpg") || n.endsWith(".jpeg")) return "image/jpeg";
    if (n.endsWith(".png")) return "image/png";
    if (n.endsWith(".gif")) return "image/gif";
    if (n.endsWith(".webp")) return "image/webp";
    return "application/octet-stream";
}

void sendFile(OutputStream out, File f) {
    sendHead(out, 200, contentType(f.getName()), f.length());
    FileInputStream in = new FileInputStream(f);
    try {
        byte[] buf = new byte[65536];
        int r;
        while ((r = in.read(buf)) > 0) out.write(buf, 0, r);
    } finally {
        in.close();
    }
    out.flush();
}

void requireAuth(Map req) {
    String h = (String) ((Map) req.get("headers")).get("authorization");
    String expected = "Bearer " + bridgeToken;
    if (h == null || !MessageDigest.isEqual(h.getBytes("UTF-8"), expected.getBytes("UTF-8"))) {
        throw new SecurityException("unauthorized");
    }
}

String q(Map req, String key, String def) {
    Object v = ((Map) req.get("query")).get(key);
    return v == null || ((String) v).length() == 0 ? def : (String) v;
}

String qRequired(Map req, String key) {
    String v = q(req, key, null);
    if (v == null) throw new IllegalArgumentException("missing query parameter " + key);
    return v;
}

long qLong(Map req, String key, long def) {
    String v = q(req, key, null);
    if (v == null) return def;
    try {
        return Long.parseLong(v);
    } catch (NumberFormatException e) {
        throw new IllegalArgumentException("bad " + key);
    }
}

List stringList(JSONObject j, String key) {
    List out = new ArrayList();
    Object v = j.opt(key);
    if (v instanceof JSONArray) {
        JSONArray a = (JSONArray) v;
        for (int i = 0; i < a.length(); i++) out.add(a.getString(i));
    } else if (v != null && v != JSONObject.NULL) {
        out.add(String.valueOf(v));
    }
    if (out.isEmpty()) throw new IllegalArgumentException("missing " + key);
    return out;
}

String optString(JSONObject j, String key) {
    return j.has(key) && !j.isNull(key) ? j.getString(key) : null;
}

// A local file argument must be something uploaded through /upload, so the API
// cannot be used to send arbitrary files off the device.
String uploadedPath(JSONObject j, String key) {
    String p = j.getString(key);
    File f = new File(p).getCanonicalFile();
    if (!f.getParent().equals(new File(uploadDir).getCanonicalPath()) || !f.isFile()) {
        throw new IllegalArgumentException(key + " must be a path returned by /upload");
    }
    return f.getAbsolutePath();
}

// ---------------------------------------------------------------------------
// Bridge: routes
// ---------------------------------------------------------------------------

Object route(Map req) {
    String method = (String) req.get("method");
    String path = (String) req.get("path");
    if (path.length() > 1 && path.endsWith("/")) path = path.substring(0, path.length() - 1);

    if (path.equals("/health")) {
        JSONObject j = ok();
        j.put("version", PLUGIN_VERSION);
        return j;
    }
    requireAuth(req);

    if (method.equals("GET")) {
        if (path.equals("/status")) return status();
        if (path.equals("/events")) return events(req);
        if (path.equals("/history")) {
            return messagesJson(queryHistoryMsg(qRequired(req, "talker"), qLong(req, "start", 0L),
                !q(req, "order", "asc").equals("desc"), (int) Math.min(qLong(req, "count", 100L), 1000L)));
        }
        if (path.equals("/message")) return findMessageJson(req);
        if (path.equals("/image")) return image(req);
        if (path.equals("/friends")) return beansJson(getFriendList());
        if (path.equals("/officials")) return beansJson(getOfficialList());
        if (path.equals("/groups")) return beansJson(getGroupList());
        if (path.equals("/group/members")) {
            String room = qRequired(req, "room");
            JSONObject j = ok();
            j.put("room", room);
            j.put("count", getGroupMemberCount(room));
            j.put("members", beansJson(getGroupMemberList(room)));
            return j;
        }
        if (path.equals("/contact")) return contact(req);
        if (path.equals("/labels")) return beansJson(getContactLabelList());
        if (path.equals("/ui/dump")) return uiDump();
        if (path.equals("/ui/selftest")) return uiSelftest();
        if (path.equals("/label/contacts")) {
            String id = q(req, "id", null);
            return beansJson(id != null ? getContactByLabelId(id) : getContactByLabelName(qRequired(req, "name")));
        }
        throw new java.io.FileNotFoundException("no route GET " + path);
    }
    if (!method.equals("POST") && !method.equals("PUT")) throw new IllegalArgumentException("unsupported method " + method);
    if (path.equals("/upload")) return upload(req, (InputStream) req.get("stream"));
    JSONObject b = (JSONObject) req.get("json");
    if (path.startsWith("/send/")) return send(path.substring(6), b);
    if (path.equals("/ui/open")) return uiLocked("open", b);
    if (path.equals("/ui/close")) return uiLocked("close", b);
    if (path.equals("/msg/revoke")) {
        revokeMsg(b.getLong("msg_id"));
        return ok();
    }
    if (path.equals("/msg/system")) {
        long createTime = b.has("create_time") ? b.getLong("create_time") : System.currentTimeMillis();
        JSONObject j = ok();
        j.put("msg_id", insertSystemMsg(b.getString("talker"), b.getString("content"), createTime));
        return j;
    }
    if (path.equals("/friend/verify")) {
        if (b.has("privacy")) verifyUser(b.getString("wxid"), b.getString("ticket"), b.getInt("scene"), b.getInt("privacy"));
        else verifyUser(b.getString("wxid"), b.getString("ticket"), b.getInt("scene"));
        return ok();
    }
    if (path.startsWith("/group/members/")) return groupMembers(path.substring(15), b);
    if (path.equals("/contact/labels")) {
        modifyContactLabelList(b.getString("wxid"), stringList(b, "labels"));
        return ok();
    }
    if (path.equals("/sns/post")) {
        String content = b.optString("content", "");
        if (b.has("images")) {
            JSONArray a = b.getJSONArray("images");
            List paths = new ArrayList();
            for (int i = 0; i < a.length(); i++) {
                JSONObject one = new JSONObject();
                one.put("p", a.getString(i));
                paths.add(uploadedPath(one, "p"));
            }
            uploadTextAndPicList(content, paths);
        } else {
            uploadText(content);
        }
        return ok();
    }
    if (path.equals("/audio/mp3-to-silk")) {
        String src = uploadedPath(b, "path");
        File dst = new File(uploadDir, System.currentTimeMillis() + "-" + new File(src).getName() + ".silk");
        JSONObject j = ok();
        j.put("code", mp3ToSilk(src, dst.getAbsolutePath()));
        j.put("path", dst.getAbsolutePath());
        return j;
    }
    if (path.equals("/toast")) {
        toast(b.getString("text"));
        return ok();
    }
    if (path.equals("/device-step")) {
        uploadDeviceStep(b.getLong("step"));
        return ok();
    }
    throw new java.io.FileNotFoundException("no route " + method + " " + path);
}

JSONObject status() {
    JSONObject j = ok();
    j.put("version", PLUGIN_VERSION);
    j.put("started_at", startedAt);
    j.put("last_seq", lastSeq.get());
    j.put("login_wxid", getLoginWxid());
    j.put("login_alias", getLoginAlias());
    j.put("host_ver_name", hostVerName);
    j.put("host_ver_code", hostVerCode);
    j.put("inbox_enabled", getBoolean("inbox_enabled", true));
    j.put("inbox_dir", inboxDir);
    j.put("bridge_dir", bridgeDir);
    return j;
}

String events(Map req) {
    long after = qLong(req, "after", 0L);
    int limit = (int) Math.max(1L, Math.min(qLong(req, "limit", 200L), 1000L));
    long wait = Math.max(0L, Math.min(qLong(req, "wait", 0L), 60L)) * 1000L;
    List lines = readEvents(after, limit, wait);
    // Built by hand: the lines are already JSON, so re-parsing them is wasted work.
    StringBuilder b = new StringBuilder("{\"ok\":true,\"last_seq\":").append(lastSeq.get()).append(",\"events\":[");
    for (int i = 0; i < lines.size(); i++) {
        if (i > 0) b.append(',');
        b.append((String) lines.get(i));
    }
    return b.append("]}").toString();
}

// The message `msg_id` in conversation `talker`; `create_time` (ms) narrows the
// history query to where the message is.
Object findMessage(Map req) {
    String talker = qRequired(req, "talker");
    long msgId = Long.parseLong(qRequired(req, "msg_id"));
    long createTime = qLong(req, "create_time", 0L);
    long start = createTime > 0 ? createTime - 1000L : 0L;
    for (int round = 0; round < 50; round++) {
        List page = queryHistoryMsg(talker, start, true, PAGE_SIZE);
        if (page == null || page.isEmpty()) break;
        long lastTime = start;
        for (int i = 0; i < page.size(); i++) {
            Object m = page.get(i);
            if (m.getMsgId() == msgId) return m;
            lastTime = Math.max(lastTime, m.getCreateTime());
        }
        if (page.size() < PAGE_SIZE || lastTime <= start || createTime > 0) break;
        start = lastTime;
    }
    throw new java.io.FileNotFoundException("message " + msgId + " not found in " + talker);
}

JSONObject findMessageJson(Map req) {
    JSONObject j = ok();
    j.put("message", messageJson(findMessage(req)));
    return j;
}

// The original of an image, either by message (talker + msg_id [+ create_time])
// or straight from its CDN fields (md5 + cdn_url + aes_key).
File image(Map req) {
    File tmp = new File(uploadDir, "img-" + System.nanoTime() + ".part");
    String md5 = q(req, "md5", null);
    if (md5 != null) {
        downloadImg(md5, qRequired(req, "cdn_url"), qRequired(req, "aes_key"), tmp.getAbsolutePath());
        if (!waitStable(tmp)) {
            tmp.delete();
            throw new RuntimeException("download produced no bytes");
        }
        return finishImage(tmp, "img-" + System.nanoTime());
    }
    Object msg = findMessage(req);
    if (!msg.isImage()) throw new IllegalArgumentException("message is not an image");
    String[] via = new String[1];
    File done = fetchOriginal(msg, tmp, via);
    if (done == null) throw new RuntimeException("download failed");
    return done;
}

JSONObject contact(Map req) {
    String wxid = qRequired(req, "wxid");
    String room = q(req, "room", null);
    JSONObject j = ok();
    j.put("wxid", wxid);
    j.put("nick_name", getFriendNickName(wxid));
    j.put("remark_name", getFriendRemarkName(wxid));
    j.put("name", room == null ? getFriendName(wxid) : getFriendName(wxid, room));
    if (room != null) j.put("display_name", getFriendDisplayName(wxid, room));
    j.put("avatar_url", getAvatarUrl(wxid, false));
    j.put("avatar_big_url", getAvatarUrl(wxid, true));
    return j;
}

JSONObject upload(Map req, InputStream in) {
    long len = ((Long) req.get("length")).longValue();
    if (len <= 0) throw new IllegalArgumentException("empty upload");
    if (len > MAX_UPLOAD) throw new IllegalArgumentException("upload too large");
    String name = q(req, "name", "upload.bin").replaceAll("[^A-Za-z0-9._-]", "_");
    pruneUploads();
    File dest = new File(uploadDir, System.currentTimeMillis() + "-" + name);
    FileOutputStream out = new FileOutputStream(dest);
    try {
        byte[] buf = new byte[65536];
        long left = len;
        while (left > 0) {
            int r = in.read(buf, 0, (int) Math.min(buf.length, left));
            if (r < 0) throw new IllegalArgumentException("truncated upload");
            out.write(buf, 0, r);
            left -= r;
        }
    } catch (Throwable e) {
        out.close();
        dest.delete();
        throw e;
    }
    out.close();
    JSONObject j = ok();
    j.put("path", dest.getAbsolutePath());
    j.put("size", dest.length());
    return j;
}

void pruneUploads() {
    File[] files = new File(uploadDir).listFiles();
    if (files == null) return;
    long cutoff = System.currentTimeMillis() - UPLOAD_TTL_MS;
    for (int i = 0; i < files.length; i++) {
        if (files[i].lastModified() < cutoff) files[i].delete();
    }
}

JSONObject groupMembers(String action, JSONObject b) {
    String room = b.getString("room");
    List members = stringList(b, "members");
    String reason = optString(b, "reason");
    if (action.equals("add")) {
        if (reason != null) addChatroomMember(room, members, reason);
        else addChatroomMember(room, members);
    } else if (action.equals("invite")) {
        if (reason != null) inviteChatroomMember(room, members, reason);
        else inviteChatroomMember(room, members);
    } else if (action.equals("remove")) {
        delChatroomMember(room, members);
    } else {
        throw new java.io.FileNotFoundException("no group action " + action);
    }
    return ok();
}

// Sends a text and waits briefly for WeChat's server id. The id is null when
// WeChat reports a failure or the callback does not arrive in time.
Object sendTextAndWait(String talker, String content) {
    final java.util.concurrent.CountDownLatch latch = new java.util.concurrent.CountDownLatch(1);
    final java.util.concurrent.atomic.AtomicReference ref = new java.util.concurrent.atomic.AtomicReference();
    sendText(talker, content, new java.util.function.Consumer() {
        public void accept(Object v) {
            ref.set(v);
            latch.countDown();
        }
    });
    latch.await(SEND_CALLBACK_MS, java.util.concurrent.TimeUnit.MILLISECONDS);
    return ref.get();
}

JSONObject send(String what, JSONObject b) {
    String talker = b.getString("talker");
    String appId = b.optString("app_id", "");
    JSONObject j = ok();
    if (what.equals("text")) {
        Object svrId = sendTextAndWait(talker, b.getString("content"));
        j.put("svr_id", svrId == null ? JSONObject.NULL : svrId);
    } else if (what.equals("quote")) {
        sendQuoteMsg(talker, b.getString("content"), b.getLong("msg_id"));
    } else if (what.equals("image")) {
        String p = uploadedPath(b, "path");
        if (b.has("quote_msg_id")) sendImage(talker, p, b.getLong("quote_msg_id"));
        else sendImage(talker, p);
    } else if (what.equals("video")) {
        sendVideo(talker, uploadedPath(b, "path"));
    } else if (what.equals("emoji")) {
        String p = uploadedPath(b, "path");
        if (b.has("quote_msg_id")) sendEmoji(talker, p, b.getLong("quote_msg_id"));
        else sendEmoji(talker, p);
    } else if (what.equals("voice")) {
        String p = uploadedPath(b, "path");
        if (b.has("duration")) sendVoice(talker, p, b.getInt("duration"));
        else sendVoice(talker, p);
    } else if (what.equals("file")) {
        String p = uploadedPath(b, "path");
        shareFile(talker, b.optString("title", new File(p).getName()), p, appId);
    } else if (what.equals("link")) {
        shareWebpage(talker, b.getString("title"), b.optString("description", ""), b.getString("url"), null, appId);
    } else if (what.equals("share-text")) {
        shareText(talker, b.getString("text"), appId);
    } else if (what.equals("share-video")) {
        shareVideo(talker, b.getString("title"), b.optString("description", ""), b.getString("url"), null, appId);
    } else if (what.equals("music")) {
        shareMusic(talker, b.getString("title"), b.optString("description", ""), b.getString("url"),
            b.getString("data_url"), null, appId);
    } else if (what.equals("mini-program")) {
        shareMiniProgram(talker, b.getString("title"), b.optString("description", ""), b.getString("user_name"),
            b.getString("page_path"), null, appId);
    } else if (what.equals("app-brand")) {
        sendAppBrandMsg(talker, b.getString("title"), b.getString("page_path"), b.getString("gh_name"));
    } else if (what.equals("pat")) {
        sendPat(talker, b.getString("user"));
    } else if (what.equals("card")) {
        sendShareCard(talker, b.getString("wxid"));
    } else if (what.equals("location")) {
        sendLocation(talker, b.getString("poi_name"), b.optString("label", ""), b.getString("x"), b.getString("y"),
            b.optString("scale", "16"));
    } else if (what.equals("note")) {
        sendNoteMsg(talker, b.getString("content"));
    } else if (what.equals("cipher")) {
        sendCipherMsg(talker, b.getString("title"), b.getString("content"));
    } else {
        throw new java.io.FileNotFoundException("no send type " + what);
    }
    return j;
}

// ---------------------------------------------------------------------------
// UI: opening a message with taps, the way a person would
// ---------------------------------------------------------------------------
//
// fav reads some messages (shared links, 贴图, 视频号 videos) only after WeChat
// has opened them. Rather than starting WeChat's activities with intents, the
// plugin taps through WeChat's own screens: back to the chat list, the chat,
// the message bubble, and later Back again. Touches are injected with
// Instrumentation, which the input system accepts for windows of the injecting
// app; if it refuses, they are dispatched into the activity's window instead.
//
// Views are found by what they hold rather than by WeChat's obfuscated class
// names: a message bubble's tag carries the message (an object with a public
// `field_msgId`), a chat list row may carry the conversation (`field_username`).
// When tags do not tell, visible text does: the contact's name in the chat
// list, the card's title in the chat. GET /ui/dump shows what the plugin sees.

java.util.concurrent.locks.ReentrantLock uiLock = new java.util.concurrent.locks.ReentrantLock();
java.util.Random uiRandom = new java.util.Random();
java.util.concurrent.atomic.AtomicBoolean uiDispatchOnly = new java.util.concurrent.atomic.AtomicBoolean(false);
Map uiFieldCache = new java.util.concurrent.ConcurrentHashMap();
int UI_MAX_STEPS = 10;
// A chat is scrolled until the message shows, however far back it is; these only
// stop a runaway.
int UI_MAX_CHAT_SCROLLS = 400;
int UI_MAX_LIST_SCROLLS = 20;
// Swipes in a row that change nothing on screen: the end of the chat.
int UI_STUCK_SWIPES = 3;
int UI_TAG_DEPTH = 3;
int UI_TAG_BUDGET = 80;
int UI_DUMP_LIMIT = 400;

void uiSleep(long min, long max) {
    Thread.sleep(min + (long) (uiRandom.nextDouble() * (max - min)));
}

Object onUi(java.util.concurrent.Callable c) {
    java.util.concurrent.FutureTask task = new java.util.concurrent.FutureTask(c);
    new android.os.Handler(android.os.Looper.getMainLooper()).post(task);
    return task.get(10, java.util.concurrent.TimeUnit.SECONDS);
}

// Every field of a class and its superclasses, made accessible, cached per class.
List uiFields(Class c) {
    List fields = (List) uiFieldCache.get(c);
    if (fields != null) return fields;
    fields = new ArrayList();
    Class k = c;
    while (k != null && k != Object.class) {
        java.lang.reflect.Field[] declared = k.getDeclaredFields();
        for (int i = 0; i < declared.length; i++) {
            if (java.lang.reflect.Modifier.isStatic(declared[i].getModifiers())) continue;
            try {
                declared[i].setAccessible(true);
                fields.add(declared[i]);
            } catch (Throwable e) {}
        }
        k = k.getSuperclass();
    }
    uiFieldCache.put(c, fields);
    return fields;
}

// Looks through `o` and the WeChat objects it refers to (up to UI_TAG_DEPTH deep)
// for the fields named in `wanted`; fills `found` with the first value of each,
// and `<name>#depth` with how much depth was left where it was found (higher is
// nearer the tag: a bubble's own message, not one its tag merely reaches).
void uiScan(Object o, String[] wanted, Map found, java.util.IdentityHashMap seen, int depth, int[] budget) {
    if (o == null || depth < 0 || budget[0] <= 0 || seen.containsKey(o)) return;
    String cls = o.getClass().getName();
    if (!cls.startsWith("com.tencent.mm")) return;
    seen.put(o, Boolean.TRUE);
    budget[0]--;
    List fields = uiFields(o.getClass());
    for (int i = 0; i < fields.size(); i++) {
        java.lang.reflect.Field f = (java.lang.reflect.Field) fields.get(i);
        for (int w = 0; w < wanted.length; w++) {
            if (f.getName().equals(wanted[w]) && !found.containsKey(wanted[w])) {
                try {
                    Object v = f.get(o);
                    if (v != null) {
                        found.put(wanted[w], v);
                        found.put(wanted[w] + "#depth", Integer.valueOf(depth));
                    }
                } catch (Throwable e) {}
            }
        }
    }
    if (found.size() == 2 * wanted.length) return;
    for (int i = 0; i < fields.size(); i++) {
        java.lang.reflect.Field f = (java.lang.reflect.Field) fields.get(i);
        if (f.getType().isPrimitive()) continue;
        try {
            Object v = f.get(o);
            if (v != null && !(v instanceof android.view.View)) uiScan(v, wanted, found, seen, depth - 1, budget);
        } catch (Throwable e) {}
        if (budget[0] <= 0) return;
    }
}

String[] UI_TAG_FIELDS = new String[] {"field_msgId", "field_talker", "field_createTime", "field_username"};

Map uiTagInfo(android.view.View v) {
    Map found = new HashMap();
    Object tag = v.getTag();
    if (tag == null) return found;
    int[] budget = new int[] {UI_TAG_BUDGET};
    uiScan(tag, UI_TAG_FIELDS, found, new java.util.IdentityHashMap(), UI_TAG_DEPTH, budget);
    return found;
}

String uiText(android.view.View v) {
    Object t = null;
    if (v instanceof android.widget.TextView) t = ((android.widget.TextView) v).getText();
    else t = getter(v, "getText");
    if (t == null || String.valueOf(t).trim().length() == 0) t = v.getContentDescription();
    if (t == null) return null;
    String s = String.valueOf(t).trim();
    return s.length() == 0 ? null : s;
}

// What is on screen in the top activity: one entry per shown view, in screen
// coordinates, with its text and what its tag holds. Runs on the UI thread.
Map uiSnapshot() {
    final android.app.Activity act = getTopActivity();
    Map snap = new HashMap();
    List nodes = new ArrayList();
    snap.put("nodes", nodes);
    snap.put("activity", act == null ? null : act.getClass().getName());
    snap.put("focused", Boolean.FALSE);
    if (act == null) return snap;
    final Map s = snap;
    java.util.concurrent.Callable uiTask = new java.util.concurrent.Callable() {
        public Object call() {
            android.view.View decor = act.getWindow().getDecorView();
            s.put("focused", Boolean.valueOf(act.hasWindowFocus()));
            s.put("width", Integer.valueOf(decor.getWidth()));
            s.put("height", Integer.valueOf(decor.getHeight()));
            int[] origin = new int[2];
            decor.getLocationOnScreen(origin);
            List stack = new ArrayList();
            stack.add(decor);
            List out = (List) s.get("nodes");
            while (!stack.isEmpty()) {
                android.view.View v = (android.view.View) stack.remove(stack.size() - 1);
                if (!v.isShown()) continue;
                android.graphics.Rect r = new android.graphics.Rect();
                if (!v.getGlobalVisibleRect(r) || r.isEmpty()) continue;
                r.offset(origin[0], origin[1]);
                Map node = new HashMap();
                node.put("view", v);
                node.put("rect", r);
                node.put("text", uiText(v));
                node.put("clickable", Boolean.valueOf(v.isClickable()));
                node.putAll(uiTagInfo(v));
                out.add(node);
                if (v instanceof android.view.ViewGroup) {
                    android.view.ViewGroup g = (android.view.ViewGroup) v;
                    for (int i = g.getChildCount() - 1; i >= 0; i--) stack.add(g.getChildAt(i));
                }
            }
            return null;
        }
    };
    onUi(uiTask);
    return snap;
}

int uiArea(Map node) {
    android.graphics.Rect r = (android.graphics.Rect) node.get("rect");
    return r.width() * r.height();
}

boolean uiIsAvatarSized(Map node) {
    android.graphics.Rect r = (android.graphics.Rect) node.get("rect");
    float density = hostContext.getResources().getDisplayMetrics().density;
    return Math.abs(r.width() - r.height()) <= 4 * density && r.width() <= 64 * density;
}

// The best view to tap among `nodes` (all carrying the same `key`): those that
// carry it nearest their tag, clickable ones first, never one the size of an
// avatar (tapping that opens a profile) or of half the screen (a container),
// the largest of what is left.
Map uiPickTarget(Map snap, List nodes, String key) {
    int screen = ((Integer) snap.get("width")).intValue() * ((Integer) snap.get("height")).intValue();
    int nearest = -1;
    for (int i = 0; i < nodes.size(); i++) {
        Object d = ((Map) nodes.get(i)).get(key + "#depth");
        if (d != null) nearest = Math.max(nearest, ((Integer) d).intValue());
    }
    Map best = null;
    for (int pass = 0; pass < 2 && best == null; pass++) {
        for (int i = 0; i < nodes.size(); i++) {
            Map n = (Map) nodes.get(i);
            Object d = n.get(key + "#depth");
            if (d != null && ((Integer) d).intValue() < nearest) continue;
            if (pass == 0 && !Boolean.TRUE.equals(n.get("clickable"))) continue;
            if (uiIsAvatarSized(n) || uiArea(n) > screen / 2) continue;
            if (best == null || uiArea(n) > uiArea(best)) best = n;
        }
    }
    return best;
}

// The nearest clickable ancestor of a node, as a node, or the node itself.
Map uiClickableAncestor(Map snap, Map node) {
    List nodes = (List) snap.get("nodes");
    android.view.View v = (android.view.View) node.get("view");
    android.view.ViewParent p = v;
    for (int depth = 0; depth < 8 && p instanceof android.view.View; depth++) {
        android.view.View pv = (android.view.View) p;
        if (pv.isClickable()) {
            for (int i = 0; i < nodes.size(); i++) {
                Map n = (Map) nodes.get(i);
                if (n.get("view") == pv) return n;
            }
        }
        p = pv.getParent();
    }
    return node;
}

List uiNodesWith(Map snap, String key, Object value) {
    List out = new ArrayList();
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size(); i++) {
        Map n = (Map) nodes.get(i);
        Object v = n.get(key);
        if (v != null && String.valueOf(v).equals(String.valueOf(value))) out.add(n);
    }
    return out;
}

// The lowest node whose text is one of `texts` and whose top edge lies between
// the two fractions of the screen's height (the title bar is the top tenth).
Map uiTextNode(Map snap, List texts, float fromTop, float toTop) {
    Map best = null;
    List nodes = (List) snap.get("nodes");
    int height = ((Integer) snap.get("height")).intValue();
    for (int i = 0; i < nodes.size(); i++) {
        Map n = (Map) nodes.get(i);
        String t = (String) n.get("text");
        if (t == null || !texts.contains(t)) continue;
        android.graphics.Rect r = (android.graphics.Rect) n.get("rect");
        if (r.top < height * fromTop || r.top > height * toTop) continue;
        if (best == null || r.top > ((android.graphics.Rect) best.get("rect")).top) best = n;
    }
    return best;
}

boolean uiInChat(Map snap) {
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size(); i++) {
        if (((Map) nodes.get(i)).containsKey("field_msgId")) return true;
    }
    return false;
}

String uiChatTalker(Map snap) {
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size(); i++) {
        Map n = (Map) nodes.get(i);
        if (n.containsKey("field_msgId") && n.get("field_talker") != null) return String.valueOf(n.get("field_talker"));
    }
    return null;
}

// [oldest, newest] send time (ms) of the messages on screen, or null when their
// views do not tell.
long[] uiVisibleTimes(Map snap) {
    long[] range = null;
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size(); i++) {
        Map n = (Map) nodes.get(i);
        Object t = n.get("field_createTime");
        if (!n.containsKey("field_msgId") || !(t instanceof Number)) continue;
        long v = ((Number) t).longValue();
        if (range == null) range = new long[] {v, v};
        range[0] = Math.min(range[0], v);
        range[1] = Math.max(range[1], v);
    }
    return range;
}

// The messages on screen, as one string, to notice a swipe that moved nothing.
String uiVisibleIds(Map snap) {
    java.util.TreeSet ids = new java.util.TreeSet();
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size(); i++) {
        Map n = (Map) nodes.get(i);
        if (n.containsKey("field_msgId")) ids.add(String.valueOf(n.get("field_msgId")));
        else if (n.get("text") != null) ids.add("t:" + n.get("text"));
    }
    return ids.toString();
}

int uiTouchDevice() {
    int[] ids = android.view.InputDevice.getDeviceIds();
    for (int i = 0; i < ids.length; i++) {
        android.view.InputDevice d = android.view.InputDevice.getDevice(ids[i]);
        if (d != null && (d.getSources() & android.view.InputDevice.SOURCE_TOUCHSCREEN) == android.view.InputDevice.SOURCE_TOUCHSCREEN) {
            return ids[i];
        }
    }
    return 0;
}

android.view.MotionEvent uiTouch(long downTime, int action, float x, float y) {
    android.view.MotionEvent.PointerProperties[] props = new android.view.MotionEvent.PointerProperties[1];
    props[0] = new android.view.MotionEvent.PointerProperties();
    props[0].id = 0;
    props[0].toolType = android.view.MotionEvent.TOOL_TYPE_FINGER;
    android.view.MotionEvent.PointerCoords[] coords = new android.view.MotionEvent.PointerCoords[1];
    coords[0] = new android.view.MotionEvent.PointerCoords();
    coords[0].x = x;
    coords[0].y = y;
    coords[0].pressure = 0.35f + uiRandom.nextFloat() * 0.4f;
    coords[0].size = 0.05f + uiRandom.nextFloat() * 0.05f;
    coords[0].touchMajor = 12f + uiRandom.nextFloat() * 8f;
    coords[0].touchMinor = coords[0].touchMajor * (0.8f + uiRandom.nextFloat() * 0.2f);
    return android.view.MotionEvent.obtain(downTime, android.os.SystemClock.uptimeMillis(), action, 1, props, coords, 0, 0,
        1f, 1f, uiTouchDevice(), 0, android.view.InputDevice.SOURCE_TOUCHSCREEN, 0);
}

// Delivers one touch event: through the input system, or, once that has been
// refused, straight into the top activity's window.
void uiSend(android.view.MotionEvent ev) {
    if (!uiDispatchOnly.get()) {
        try {
            new android.app.Instrumentation().sendPointerSync(ev);
            ev.recycle();
            return;
        } catch (RuntimeException e) {
            // SecurityException or IllegalStateException: injection needs INJECT_EVENTS after all.
            log("FavBridge cannot inject touches (" + e + "); dispatching them into the window instead");
            uiDispatchOnly.set(true);
        }
    }
    final android.app.Activity act = getTopActivity();
    final android.view.MotionEvent event = ev;
    if (act == null) throw new IllegalStateException("WeChat has no activity on screen");
    java.util.concurrent.Callable uiTask = new java.util.concurrent.Callable() {
        public Object call() {
            act.dispatchTouchEvent(event);
            event.recycle();
            return null;
        }
    };
    onUi(uiTask);
}

// A tap somewhere inside the middle half of a view, held as long as a finger would.
void uiTap(android.graphics.Rect r) {
    float x = r.left + r.width() * (0.25f + uiRandom.nextFloat() * 0.5f);
    float y = r.top + r.height() * (0.25f + uiRandom.nextFloat() * 0.5f);
    long down = android.os.SystemClock.uptimeMillis();
    uiSend(uiTouch(down, android.view.MotionEvent.ACTION_DOWN, x, y));
    uiSleep(40, 80);
    uiSend(uiTouch(down, android.view.MotionEvent.ACTION_MOVE, x + uiRandom.nextFloat() * 2f, y + uiRandom.nextFloat() * 2f));
    uiSleep(30, 70);
    uiSend(uiTouch(down, android.view.MotionEvent.ACTION_UP, x + uiRandom.nextFloat() * 2f, y + uiRandom.nextFloat() * 2f));
}

// A vertical swipe across the middle of the screen; a positive `fraction` drags
// the content down, which shows older messages or the top of a list.
void uiSwipe(Map snap, float fraction) {
    int width = ((Integer) snap.get("width")).intValue();
    int height = ((Integer) snap.get("height")).intValue();
    float x = width * (0.4f + uiRandom.nextFloat() * 0.2f);
    float y0 = height * (fraction > 0 ? 0.35f : 0.7f);
    float y1 = y0 + height * fraction;
    long down = android.os.SystemClock.uptimeMillis();
    uiSend(uiTouch(down, android.view.MotionEvent.ACTION_DOWN, x, y0));
    int moves = 8 + uiRandom.nextInt(5);
    for (int i = 1; i <= moves; i++) {
        Thread.sleep(25);
        uiSend(uiTouch(down, android.view.MotionEvent.ACTION_MOVE, x + uiRandom.nextFloat() * 3f, y0 + (y1 - y0) * i / moves));
    }
    uiSend(uiTouch(down, android.view.MotionEvent.ACTION_UP, x, y1));
}

// Back, as the navigation bar would send it. Returns how it was delivered.
String uiBack() {
    if (!uiDispatchOnly.get()) {
        try {
            new android.app.Instrumentation().sendKeyDownUpSync(android.view.KeyEvent.KEYCODE_BACK);
            return "key";
        } catch (RuntimeException e) {
            log("FavBridge cannot inject Back (" + e + ")");
            uiDispatchOnly.set(true);
        }
    }
    final android.app.Activity act = getTopActivity();
    if (act == null || !act.hasWindowFocus()) {
        // What is on top lives in another WeChat process (the web view); bring the chat list back over it.
        android.content.Intent home = hostContext.getPackageManager().getLaunchIntentForPackage(hostContext.getPackageName());
        home.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK | android.content.Intent.FLAG_ACTIVITY_CLEAR_TOP);
        hostContext.startActivity(home);
        return "launcher";
    }
    java.util.concurrent.Callable uiTask = new java.util.concurrent.Callable() {
        public Object call() {
            act.onBackPressed();
            return null;
        }
    };
    onUi(uiTask);
    return "activity";
}

boolean uiIsMainScreen(android.app.Activity act) {
    if (act == null) return false;
    String name = act.getClass().getName();
    return name.endsWith(".LauncherUI") || name.indexOf("ChattingUI") >= 0;
}

// Brings WeChat to the front the way its launcher icon does.
void uiForeground(List steps) {
    android.app.Activity act = getTopActivity();
    if (act != null && act.hasWindowFocus()) return;
    if (act != null && !uiIsMainScreen(act)) return;
    android.content.Intent launch = hostContext.getPackageManager().getLaunchIntentForPackage(hostContext.getPackageName());
    launch.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
    hostContext.startActivity(launch);
    steps.add("brought WeChat to the front");
    uiSleep(1500, 2500);
}

String uiName(String talker, String which) {
    try {
        Object v = which.equals("remark") ? getFriendRemarkName(talker) : which.equals("nick") ? getFriendNickName(talker) : getFriendName(talker);
        return v == null ? null : String.valueOf(v).trim();
    } catch (Throwable e) {
        return null;
    }
}

// The message's bubble on screen: by the message its view carries, else by the
// text it shows.
Map uiFindMessage(Map snap, long msgId, List texts, String[] foundBy) {
    Map target = uiPickTarget(snap, uiNodesWith(snap, "field_msgId", Long.valueOf(msgId)), "field_msgId");
    if (target != null) {
        foundBy[0] = "tag";
        return target;
    }
    if (texts.isEmpty()) return null;
    Map textNode = uiTextNode(snap, texts, 0.1f, 0.95f);
    if (textNode == null) return null;
    foundBy[0] = "text";
    return uiClickableAncestor(snap, textNode);
}

// Whether a tap on the bubble opened something: the chat no longer has the
// focus, or another activity took over.
boolean uiLeftChat(String chatActivity) {
    android.app.Activity act = getTopActivity();
    if (act == null || !act.hasWindowFocus()) return true;
    return !act.getClass().getName().equals(chatActivity);
}

// Taps through to message `msgId` of chat `talker` and opens it. `createTime`
// (ms, 0 when unknown) says which way to scroll the chat; `texts` are what its
// bubble may show (a card's title), for when its view does not carry it. The
// chat is scrolled until the message shows, however old it is, and only gives
// up at the end of the chat.
JSONObject uiOpenMessage(String talker, long msgId, long createTime, List texts) {
    List steps = new ArrayList();
    List names = new ArrayList();
    String[] kinds = new String[] {"name", "remark", "nick"};
    for (int i = 0; i < kinds.length; i++) {
        String n = uiName(talker, kinds[i]);
        if (n != null && n.length() > 0 && !names.contains(n)) names.add(n);
    }
    boolean opened = false;
    boolean done = false;
    String[] foundBy = new String[1];
    int nav = 0;
    int taps = 0;
    int listScrolls = 0;
    int scrollsUp = 0;
    int scrollsDown = 0;
    int stuck = 0;
    int straddled = 0;
    float lastDirection = 0f;
    String lastScreen = null;
    boolean tappedHomeTab = false;
    uiForeground(steps);
    // One action per pass, chosen by an if/else chain: WAuxiliary's BeanShell
    // is not trusted with break and continue here.
    while (!done && !opened && nav < UI_MAX_STEPS) {
        Map snap = uiSnapshot();
        android.app.Activity act = getTopActivity();
        boolean elsewhere = !Boolean.TRUE.equals(snap.get("focused")) || !uiIsMainScreen(act);
        String inChat = elsewhere ? null : uiChatTalker(snap);
        boolean otherChat = inChat != null && !inChat.equals(talker);
        Map target = elsewhere || otherChat ? null : uiFindMessage(snap, msgId, texts, foundBy);
        if (elsewhere) {
            steps.add("Back from " + snap.get("activity") + " (" + uiBack() + ")");
            nav++;
            uiSleep(700, 1200);
        } else if (otherChat) {
            steps.add("Back from another chat (" + uiBack() + ")");
            nav++;
            uiSleep(600, 1000);
        } else if (target != null) {
            if (scrollsUp + scrollsDown > 0) steps.add("scrolled the chat " + scrollsUp + " times up, " + scrollsDown + " down");
            uiSleep(400, 900);
            uiTap((android.graphics.Rect) target.get("rect"));
            taps++;
            nav++;
            steps.add("tapped the message (found by " + foundBy[0] + (uiDispatchOnly.get() ? ", dispatched" : ", injected") + ")");
            uiSleep(1500, 2500);
            if (uiLeftChat(act.getClass().getName())) {
                opened = true;
            } else if (taps < 2) {
                // The tap changed nothing; deliver the next one straight into the window.
                steps.add("the chat stayed in front; tapping again");
                uiDispatchOnly.set(true);
            } else {
                done = true;
            }
        } else if (uiInChat(snap)) {
            // Older messages are above. When the send times on screen say the
            // message is below them, go back down, in shorter swipes once the
            // direction has changed so the two do not overshoot each other.
            float direction = 1f;
            boolean missing = false;
            long[] range = uiVisibleTimes(snap);
            if (createTime > 0 && range != null) {
                if (range[1] < createTime) {
                    direction = -1f;
                } else if (range[0] <= createTime) {
                    // The screen spans its send time and it is not there: hidden or deleted.
                    straddled++;
                    missing = straddled >= 2;
                }
            }
            String screen = uiVisibleIds(snap);
            stuck = screen.equals(lastScreen) ? stuck + 1 : 0;
            lastScreen = screen;
            if (missing) {
                steps.add("the chat shows the messages around it, but not this one");
                done = true;
            } else if (stuck >= UI_STUCK_SWIPES) {
                steps.add("reached the " + (direction > 0 ? "top" : "bottom") + " of the chat after " + (scrollsUp + scrollsDown) + " swipes");
                done = true;
            } else if (scrollsUp + scrollsDown >= UI_MAX_CHAT_SCROLLS) {
                steps.add("gave up after " + UI_MAX_CHAT_SCROLLS + " swipes");
                done = true;
            } else {
                boolean turned = lastDirection != 0f && direction != lastDirection;
                float size = turned || (scrollsUp > 0 && scrollsDown > 0) ? 0.2f : 0.45f;
                uiSwipe(snap, direction * size);
                lastDirection = direction;
                if (direction > 0) scrollsUp++;
                else scrollsDown++;
                // A screen that did not move may be WeChat loading older messages.
                if (stuck > 0) uiSleep(1500, 2500);
                else uiSleep(500, 900);
            }
        } else {
            // The chat list: find the conversation and tap it.
            Map row = uiPickTarget(snap, uiNodesWith(snap, "field_username", talker), "field_username");
            if (row == null) {
                Map nameNode = uiTextNode(snap, names, 0.1f, 0.9f);
                if (nameNode != null) row = uiClickableAncestor(snap, nameNode);
            }
            Map tab = null;
            if (row == null && !tappedHomeTab) tab = uiTextNode(snap, Arrays.asList(new String[] {"微信", "WeChat", "Chats"}), 0.85f, 1f);
            if (row != null) {
                uiSleep(300, 700);
                uiTap((android.graphics.Rect) row.get("rect"));
                nav++;
                steps.add("tapped the chat");
                uiSleep(900, 1500);
            } else if (tab != null) {
                uiTap((android.graphics.Rect) uiClickableAncestor(snap, tab).get("rect"));
                tappedHomeTab = true;
                nav++;
                steps.add("tapped the 微信 tab");
                uiSleep(600, 1000);
            } else if (listScrolls >= UI_MAX_LIST_SCROLLS) {
                steps.add("the chat is not in the chat list");
                done = true;
            } else {
                uiSwipe(snap, -0.35f);
                listScrolls++;
                steps.add("scrolled down the chat list");
                uiSleep(600, 1000);
            }
        }
    }
    JSONObject result = ok();
    JSONArray stepsJson = new JSONArray();
    for (int i = 0; i < steps.size(); i++) stepsJson.put(steps.get(i));
    result.put("opened", opened);
    result.put("found_by", nn(foundBy[0]));
    result.put("steps", stepsJson);
    if (!opened) {
        result.put("ok", false);
        result.put("error", "could not open message " + msgId + " of " + talker + " by tapping it; see steps and GET /ui/dump");
    }
    return result;
}

// Back until WeChat is on its chat list (or, without `home`, until the opened
// message is closed and its chat shows).
JSONObject uiClose(boolean home) {
    JSONArray steps = new JSONArray();
    boolean done = false;
    int i = 0;
    while (!done && i < UI_MAX_STEPS) {
        android.app.Activity act = getTopActivity();
        boolean focused = act != null && act.hasWindowFocus();
        if (focused && uiIsMainScreen(act) && (!home || !uiInChat(uiSnapshot()))) {
            done = true;
        } else {
            steps.put("Back from " + (focused ? act.getClass().getName() : "another window") + " (" + uiBack() + ")");
            uiSleep(700, 1200);
        }
        i++;
    }
    JSONObject j = ok();
    j.put("steps", steps);
    return j;
}

// One UI action at a time. No try/finally: the lock is released on both paths
// by hand, and an empty answer is reported rather than passed on.
JSONObject uiLocked(String what, JSONObject b) {
    if (!uiLock.tryLock(15, java.util.concurrent.TimeUnit.MINUTES)) throw new IllegalStateException("another UI action is running");
    Object[] box = new Object[1];
    Throwable failure = null;
    try {
        if (what.equals("open")) {
            List texts = new ArrayList();
            JSONArray a = b.optJSONArray("texts");
            for (int i = 0; a != null && i < a.length(); i++) {
                String t = a.optString(i, "").trim();
                if (t.length() > 0) texts.add(t);
            }
            box[0] = uiOpenMessage(b.getString("talker"), b.getLong("msg_id"), b.optLong("create_time", 0L), texts);
        } else {
            box[0] = uiClose(b.optBoolean("home", true));
        }
    } catch (Throwable e) {
        failure = e;
    }
    uiLock.unlock();
    if (failure != null) throw failure;
    if (box[0] == null) throw new IllegalStateException("ui " + what + " returned nothing");
    return (JSONObject) box[0];
}

// Which control-flow forms come back with their value in this BeanShell. Each
// probe returns "ok"; a form the interpreter mishandles shows as "nothing".
String uiProbeFinally() {
    String s = null;
    try {
        s = "ok";
    } finally {
        uiFieldCache.size();
    }
    return s;
}

String uiProbeBreak() {
    int i = 0;
    while (i < 5) {
        if (i == 2) {
            if (i > 0) break;
        }
        i++;
    }
    return "ok";
}

String uiProbeContinue() {
    int i = 0;
    int n = 0;
    while (i < 5) {
        i++;
        if (i % 2 == 0) continue;
        n++;
    }
    return "ok";
}

String uiProbeForBreak() {
    for (int i = 0; i < 5; i++) {
        if (i == 2) break;
    }
    return "ok";
}

JSONObject uiProbeLockedFinally() {
    java.util.concurrent.locks.ReentrantLock lock = new java.util.concurrent.locks.ReentrantLock();
    JSONObject result = null;
    lock.lock();
    try {
        if (lock.isHeldByCurrentThread()) {
            List l = new ArrayList();
            result = ok();
        } else {
            result = ok();
        }
    } finally {
        lock.unlock();
    }
    return result;
}

// A local captured by a scripted Callable, read back through FutureTask, as
// guarded() used to pass the request body's stream.
String uiProbeCaptured(String value) {
    final String input = value;
    java.util.concurrent.FutureTask task = new java.util.concurrent.FutureTask(new java.util.concurrent.Callable() {
        public Object call() {
            return input.length() > 0 ? "ok" : "empty";
        }
    });
    task.run();
    return (String) task.get();
}

// A script error inside a scripted Callable: "thrown" when it surfaces as an
// exception, "nothing" when the interpreter turns it into a null.
String uiProbeCallableError() {
    java.util.concurrent.FutureTask task = new java.util.concurrent.FutureTask(new java.util.concurrent.Callable() {
        public Object call() {
            String missing = null;
            return missing.trim();
        }
    });
    task.run();
    String answer = "thrown";
    try {
        if (task.get() == null) answer = "nothing";
    } catch (Throwable e) {
        answer = "thrown";
    }
    return answer;
}

JSONObject uiSelftest() {
    JSONObject j = ok();
    j.put("finally", uiProbeFinally() == null ? "nothing" : "ok");
    j.put("while_break", uiProbeBreak() == null ? "nothing" : "ok");
    j.put("while_continue", uiProbeContinue() == null ? "nothing" : "ok");
    j.put("for_break", uiProbeForBreak() == null ? "nothing" : "ok");
    j.put("lock_finally", uiProbeLockedFinally() == null ? "nothing" : "ok");
    j.put("captured", String.valueOf(uiProbeCaptured("x")));
    j.put("callable_error", uiProbeCallableError());
    return j;
}

// What the plugin sees on screen, for working out why a message was not found.
JSONObject uiDump() {
    Map snap = uiSnapshot();
    JSONObject j = ok();
    j.put("activity", nn(snap.get("activity")));
    j.put("focused", snap.get("focused"));
    j.put("touch_injection", !uiDispatchOnly.get());
    JSONArray views = new JSONArray();
    List nodes = (List) snap.get("nodes");
    for (int i = 0; i < nodes.size() && i < UI_DUMP_LIMIT; i++) {
        Map n = (Map) nodes.get(i);
        android.graphics.Rect r = (android.graphics.Rect) n.get("rect");
        JSONObject v = new JSONObject();
        android.view.View view = (android.view.View) n.get("view");
        v.put("class", view.getClass().getName());
        v.put("rect", r.toShortString());
        v.put("clickable", n.get("clickable"));
        String text = (String) n.get("text");
        if (text != null) v.put("text", text.length() > 16 ? text.substring(0, 16) + "…" : text);
        if (view.getTag() != null) v.put("tag", view.getTag().getClass().getName());
        if (n.containsKey("field_msgId")) v.put("msg_id", n.get("field_msgId"));
        if (n.containsKey("field_talker")) v.put("talker", String.valueOf(n.get("field_talker")));
        if (n.containsKey("field_username")) v.put("username", String.valueOf(n.get("field_username")));
        if (n.containsKey("field_msgId#depth")) v.put("msg_depth", n.get("field_msgId#depth"));
        if (n.containsKey("field_createTime")) v.put("create_time", n.get("field_createTime"));
        views.put(v);
    }
    j.put("views", views);
    return j;
}
