package tailcatnative

import (
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"tailscale.com/tstest/integration"
	"tailscale.com/types/logger"
)

// Keep the client alive while only the remote server loses its in-memory
// WireGuard peer map. Reopening BOTH endpoints misses this production failure.
func TestLiveClientRecoversAfterServerRestart(t *testing.T) {
	dm := integration.RunDERPAndSTUN(t, logger.Discard, "127.0.0.1")
	var mu sync.Mutex
	received := make(map[string]int)
	backend := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			http.Error(w, "body", 400)
			return
		}
		mu.Lock()
		received[string(body)]++
		mu.Unlock()
		_, _ = w.Write(body)
	}))
	defer backend.Close()
	root := t.TempDir()
	state := filepath.Join(root, "server.key")
	server, err := startServer(backend.Listener.Addr().String(), state, "", 0, dm.Regions[1])
	if err != nil {
		t.Fatal(err)
	}
	defer func() { server.Close() }()
	address := server.Address()
	clientState := filepath.Join(root, "client.key")
	client, err := startClient(address, "127.0.0.1:0", clientState, "", true)
	if err != nil {
		t.Fatal(err)
	}
	defer client.Close()
	identity, err := os.ReadFile(clientState)
	if err != nil {
		t.Fatal(err)
	}
	url := client.URL()
	// No pooled HTTP connection: every request exercises a new tunnel TCP dial.
	httpClient := &http.Client{Transport: &http.Transport{DisableKeepAlives: true}, Timeout: 15 * time.Second}
	defer httpClient.CloseIdleConnections()
	request := func(payload string) error {
		response, err := httpClient.Post(url, "text/plain", strings.NewReader(payload))
		if err != nil {
			return err
		}
		defer response.Body.Close()
		got, err := io.ReadAll(response.Body)
		if err == nil && (response.StatusCode != 200 || string(got) != payload) {
			return fmt.Errorf("incorrect response for %q", payload)
		}
		return err
	}
	if err := request("before-restart"); err != nil {
		t.Fatalf("initial request: %v", err)
	}
	for round := 0; round < 3; round++ {
		server.Close()
		restarted, err := startServer(backend.Listener.Addr().String(), state, "", 0, nil)
		if err != nil {
			t.Fatal(err)
		}
		server = restarted
		if server.Address() != address {
			t.Fatal("server identity changed")
		}
		// Registry, chat and RPC supervisors can all redial at once. Their
		// failures must coalesce rather than repeatedly killing fresh tunnels.
		results := make(chan error, 8)
		for i := 0; i < cap(results); i++ {
			payload := fmt.Sprintf("restart-%d-request-%d", round, i)
			go func() { results <- request(payload) }()
		}
		for i := 0; i < cap(results); i++ {
			if err := <-results; err != nil {
				t.Fatalf("live client did not reconnect after server-only restart: %v", err)
			}
		}
	}
	mu.Lock()
	defer mu.Unlock()
	if len(received) != 25 {
		t.Fatalf("received %d requests, want 25", len(received))
	}
	for payload, count := range received {
		if count != 1 {
			t.Fatalf("request %q was replayed %d times", payload, count)
		}
	}
	if client.URL() != url {
		t.Fatal("local listener changed during recovery")
	}
	after, err := os.ReadFile(clientState)
	if err != nil {
		t.Fatal(err)
	}
	if string(after) != string(identity) {
		t.Fatal("recovery replaced client identity")
	}
}
