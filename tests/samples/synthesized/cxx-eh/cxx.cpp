// C++ exception handling: a typed catch, a catch-all, two catches on one try, a destructor
// that must run (an unwind action), and a rethrow.
struct Error { int code; };
struct Other { int a, b; };

volatile int destroyed;
struct Guard { ~Guard() { destroyed++; } };

__attribute__((noinline)) void fail(int n) {
    if (n == 1) throw Error{n};
    if (n == 2) throw Other{n, n};
    if (n == 3) throw 3;
}

__attribute__((noinline)) int typed(int n) {
    try { fail(n); } catch (const Error &e) { return e.code; }
    return 0;
}

__attribute__((noinline)) int any(int n) {
    try { fail(n); } catch (...) { return -1; }
    return 0;
}

__attribute__((noinline)) int two(int n) {
    try { fail(n); }
    catch (const Error &e) { return e.code + 10; }
    catch (const Other &o) { return o.a + o.b; }
    return 0;
}

__attribute__((noinline)) int guarded(int n) {
    Guard g;
    fail(n);
    return n;
}

__attribute__((noinline)) int rethrows(int n) {
    try {
        try { fail(n); } catch (...) { destroyed++; throw; }
    } catch (int v) { return v; }
    return 0;
}

int main() { return typed(1) + any(2) + two(2) + guarded(0) + rethrows(3); }
