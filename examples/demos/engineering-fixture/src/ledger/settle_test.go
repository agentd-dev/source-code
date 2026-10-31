package ledger

import (
	"testing"
	"time"
)

// Payments captured just before and after midnight UTC land on two days.
func TestSettleSplitsAtMidnightUTC(t *testing.T) {
	late := time.Date(2026, 10, 30, 23, 59, 0, 0, time.UTC)
	early := time.Date(2026, 10, 31, 0, 1, 0, 0, time.UTC)
	got := Settle("m_1", map[time.Time]int64{late: 1000, early: 2500})
	if len(got) != 2 {
		t.Fatalf("want 2 settlement days, got %d", len(got))
	}
}
