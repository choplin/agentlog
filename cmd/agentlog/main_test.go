package main

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"testing"
)

func TestClipSummary(t *testing.T) {
	if got := clipSummary("abcdef", 3); got != "ab…" {
		t.Fatalf("clipSummary unexpected result: %q", got)
	}
	if got := clipSummary("short", 10); got != "short" {
		t.Fatalf("clipSummary should not alter short text: %q", got)
	}
}

func TestCollapseWhitespace(t *testing.T) {
	text := "  line one\n\nline\t two  "
	if got := collapseWhitespace(text); got != "line one line two" {
		t.Fatalf("collapseWhitespace failed: %q", got)
	}
}

func TestViewCommandFormatRaw(t *testing.T) {
	cmd := newViewCmd()
	var buf bytes.Buffer
	cmd.SetOut(&buf)
	cmd.SetErr(io.Discard)
	path := filepath.Join("..", "..", "testdata", "sessions", "sample-simple.jsonl")
	cmd.SetArgs([]string{path, "--format", "raw", "--level", "all"})
	if err := cmd.Execute(); err != nil {
		t.Fatalf("view command failed: %v", err)
	}
	wantBytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read sample file: %v", err)
	}
	// With --level all, we expect all lines including session_meta
	want := string(wantBytes)
	if got := buf.String(); got != want {
		t.Fatalf("raw output mismatch\nwant:\n%q\n\ngot:\n%q", want, got)
	}
}
