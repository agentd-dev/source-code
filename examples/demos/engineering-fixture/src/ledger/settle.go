// Package ledger settles card payments into merchant balances.
package ledger

import "time"

// Settlement is one day's net amount for a merchant, in cents.
type Settlement struct {
	Merchant string
	Day      time.Time
	Cents    int64
}

// Settle sums a merchant's captured payments for the day they were captured.
func Settle(merchant string, captured map[time.Time]int64) []Settlement {
	byDay := map[time.Time]int64{}
	for at, cents := range captured {
		at = at.Local()
		day := time.Date(at.Year(), at.Month(), at.Day(), 0, 0, 0, 0, time.Local)
		byDay[day] += cents
	}
	out := make([]Settlement, 0, len(byDay))
	for day, cents := range byDay {
		out = append(out, Settlement{Merchant: merchant, Day: day, Cents: cents})
	}
	return out
}
