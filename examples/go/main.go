// A key-value server over TCP and a client that drives it from many
// goroutines, in one binary: `kvgo server :7000` and `kvgo client host:7000`.
// The Go runtime schedules goroutines over threads of its own, preempts
// with signals and polls the network with kqueue: what the supervisor
// has to own for a Go program.
package main

import (
	"bufio"
	"fmt"
	"net"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

type store struct {
	mu   sync.Mutex
	vals map[string]int
	ops  int
}

func (s *store) apply(line string) string {
	f := strings.Fields(line)
	s.mu.Lock()
	defer s.mu.Unlock()
	s.ops++
	switch {
	case len(f) == 3 && f[0] == "incr":
		n, _ := strconv.Atoi(f[2])
		s.vals[f[1]] += n
		return strconv.Itoa(s.vals[f[1]])
	case len(f) == 2 && f[0] == "get":
		return strconv.Itoa(s.vals[f[1]])
	case len(f) == 2 && f[0] == "del":
		delete(s.vals, f[1])
		return "ok"
	case len(f) == 1 && f[0] == "sum":
		t := 0
		for _, v := range s.vals {
			t += v
		}
		return fmt.Sprintf("%d keys %d total %d ops", len(s.vals), t, s.ops)
	}
	return "?"
}

func server(addr string) {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	st := &store{vals: map[string]int{}}
	// A ticker, as servers have: expiry sweeps on a timer
	go func() {
		for range time.Tick(50 * time.Millisecond) {
			st.mu.Lock()
			for k := range st.vals {
				if strings.HasPrefix(k, "tmp:") {
					delete(st.vals, k)
				}
			}
			st.mu.Unlock()
		}
	}()
	for {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		go func() {
			defer c.Close()
			r := bufio.NewScanner(c)
			for r.Scan() {
				fmt.Fprintln(c, st.apply(r.Text()))
			}
		}()
	}
}

func client(addr string) {
	const workers, rounds = 8, 30
	var wg sync.WaitGroup
	results := make([][]string, workers)
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			var c net.Conn
			var err error
			for {
				c, err = net.Dial("tcp", addr)
				if err == nil {
					break
				}
				time.Sleep(200 * time.Millisecond)
			}
			defer c.Close()
			r := bufio.NewReader(c)
			ask := func(q string) string {
				fmt.Fprintln(c, q)
				s, _ := r.ReadString('\n')
				return strings.TrimSpace(s)
			}
			for i := 0; i < rounds; i++ {
				ask(fmt.Sprintf("incr k%d %d", i%5, w+1))
				ask(fmt.Sprintf("incr tmp:%d 1", w))
				if i%7 == 6 {
					ask(fmt.Sprintf("del k%d", (w+i)%5))
				}
				results[w] = append(results[w], ask("get k0"))
			}
			results[w] = append(results[w], ask("sum"))
		}(w)
	}
	wg.Wait()
	for w := 0; w < workers; w++ {
		seen := results[w]
		sort.Strings(seen[:len(seen)-1])
		fmt.Printf("worker %d: k0 seen %v ... final %s\n", w, seen[:4], seen[len(seen)-1])
	}
}

func main() {
	if len(os.Args) == 3 && os.Args[1] == "server" {
		server(os.Args[2])
	} else if len(os.Args) == 3 && os.Args[1] == "client" {
		client(os.Args[2])
	} else {
		fmt.Fprintln(os.Stderr, "usage: kvgo server :7000 | kvgo client host:7000")
		os.Exit(2)
	}
}
