# THROWAWAY spike TEST TOOL: failure cases against the helper.
import socket, ssl, sys, time
HOST, PORT = "127.0.0.1", 8443

def client_hello():
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    i, o = ssl.MemoryBIO(), ssl.MemoryBIO()
    obj = ctx.wrap_bio(i, o, server_hostname="localhost")
    try: obj.do_handshake()
    except ssl.SSLWantReadError: pass
    return o.read()

def recv_all(s, t=12):
    s.settimeout(t); out = b""; t0 = time.time()
    try:
        while True:
            d = s.recv(65536)
            if not d: return out, "EOF", time.time() - t0
            out += d
    except ConnectionResetError: return out, "RST", time.time() - t0
    except socket.timeout: return out, "timeout", time.time() - t0

case = sys.argv[1]
s = socket.create_connection((HOST, PORT))
if case == "silent":
    out, how, dt = recv_all(s, 30)
elif case == "garbage":
    s.sendall(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"); out, how, dt = recv_all(s)
elif case == "garbage-tlsheader":
    s.sendall(b"\x16\x03\x01\x00\x10" + b"\xff" * 16); out, how, dt = recv_all(s)
elif case == "abort":
    s.sendall(client_hello()[:100]); s.close(); print("client: sent 100 bytes of a ClientHello and closed"); sys.exit()
elif case == "hello-then-wait":
    s.sendall(client_hello()); print("client: sent ClientHello, now waiting", flush=True); out, how, dt = recv_all(s, 30)
print(f"client: got {len(out)} bytes then {how} after {dt:.2f}s; first bytes {out[:7].hex()}")
