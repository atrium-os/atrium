// S5 fixture: a C++ library linked against the real libstdc++ (Ubuntu focal 6.0.28, the
// one the InnaIT PKCS#11 module needs), loaded with it as one set (load_set) over the
// glibc shim. Exercises what such a module leans on: exceptions thrown and caught by
// libstdc++ (through the shim's unwinder), its locale and iostream start-up, std::string,
// containers, threads, mutexes, call_once (libstdc++'s own TLS, read from here) and
// thread_local. fixture_cxx_main() returns 0, or the number of the first failed check.
// Built -nostdlib against focal's headers (scripts/hospes-build.sh cxxset), so every
// glibc import is unversioned and binds to the shim's baseline row.
#include <condition_variable>
#include <iostream>
#include <map>
#include <mutex>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

#define CHECK(n, c) do { if (!(c)) return (n); } while (0)

namespace {

struct Counted {
    static int live;
    int v;
    explicit Counted(int v) : v(v) { ++live; }
    Counted(const Counted& o) : v(o.v) { ++live; }
    ~Counted() { --live; }
};
int Counted::live = 0;

int thrower(int k) {
    Counted c(k);
    if (k > 2) throw std::runtime_error("deep " + std::to_string(k));
    return thrower(k + 1) + c.v;
}

std::once_flag once;
int once_runs;
thread_local std::string tl_name = "main";

}  // namespace

extern "C" __attribute__((visibility("default"))) int fixture_cxx_main() {
    // Exceptions: thrown three frames down, objects destroyed on the way, caught here.
    try {
        thrower(0);
        return 1;
    } catch (const std::runtime_error& e) {
        CHECK(2, std::string(e.what()) == "deep 3");
    }
    CHECK(3, Counted::live == 0);
    try {
        std::vector<int> v(2);
        (void)v.at(7);
        return 4;
    } catch (const std::out_of_range&) {
    }

    // Locale and streams: the C locale's numpunct, formatting through libstdc++.
    std::ostringstream os;
    os << 42 << ' ' << 2.5 << ' ' << std::hex << 255 << ' ' << true;
    CHECK(5, os.str() == "42 2.5 ff 1");
    std::istringstream is("17 x");
    int n = 0;
    std::string w;
    is >> n >> w;
    CHECK(6, n == 17 && w == "x");

    std::map<std::string, int> m;
    for (int i = 0; i < 50; i++) m["k" + std::to_string(i)] = i;
    CHECK(7, m.size() == 50 && m["k49"] == 49);

    // Threads, a mutex and a condition variable, call_once from all of them.
    std::mutex mu;
    std::condition_variable cv;
    int done = 0;
    long sum = 0;
    std::vector<std::thread> ts;
    for (int t = 0; t < 4; t++) {
        ts.emplace_back([&, t] {
            std::call_once(once, [] { once_runs++; });
            tl_name = "worker" + std::to_string(t);
            long local = 0;
            for (int i = 0; i < 1000; i++) local += i;
            std::lock_guard<std::mutex> g(mu);
            sum += local;
            if (tl_name != "worker" + std::to_string(t)) sum = -1;
            done++;
            cv.notify_one();
        });
    }
    {
        std::unique_lock<std::mutex> g(mu);
        cv.wait(g, [&] { return done == 4; });
    }
    for (auto& t : ts) t.join();
    CHECK(8, sum == 4 * 499500);
    CHECK(9, once_runs == 1);
    CHECK(10, tl_name == "main");

    // An exception thrown on another thread, caught there.
    bool caught = false;
    std::thread([&] {
        try {
            throw std::logic_error("elsewhere");
        } catch (const std::logic_error&) {
            caught = true;
        }
    }).join();
    CHECK(11, caught);

    std::cerr << "fixture: libstdc++ " << __GLIBCXX__ << " streams reach the log" << std::endl;
    return 0;
}
