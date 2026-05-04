// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

#pragma once

#include <cstdint>

namespace flowlab {

/// Rolling statistics for microstructure metrics.
/// Online algorithm (Welford's) — no allocation, O(1) per update.
class RollingStats {
public:
    RollingStats() : count_(0), mean_(0.0), m2_(0.0), min_(0.0), max_(0.0) {}

    void update(double value) noexcept {
        count_++;
        double delta = value - mean_;
        mean_ += delta / static_cast<double>(count_);
        double delta2 = value - mean_;
        m2_ += delta * delta2;

        if (count_ == 1) {
            min_ = max_ = value;
        } else {
            if (value < min_) min_ = value;
            if (value > max_) max_ = value;
        }
    }

    uint64_t count() const noexcept { return count_; }
    double mean() const noexcept { return mean_; }
    double variance() const noexcept { return count_ > 1 ? m2_ / static_cast<double>(count_ - 1) : 0.0; }
    double min() const noexcept { return min_; }
    double max() const noexcept { return max_; }

    void reset() noexcept {
        count_ = 0;
        mean_ = m2_ = min_ = max_ = 0.0;
    }

private:
    uint64_t count_;
    double mean_;
    double m2_;
    double min_;
    double max_;
};

} // namespace flowlab
