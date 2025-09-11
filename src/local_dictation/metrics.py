#!/usr/bin/env python3
"""
Performance metrics and logging
- Detailed timing breakdowns
- P50/P95 latency tracking
- Performance guardrails
"""
from __future__ import annotations
import time
import json
from typing import Dict, List, Optional
from dataclasses import dataclass, asdict
import numpy as np
from pathlib import Path
import sys

@dataclass
class TranscriptionMetrics:
    """Metrics for a single transcription"""
    timestamp: float
    recording_duration_ms: float
    vad_processing_ms: float
    audio_processing_ms: float
    transcription_ms: float
    text_insertion_ms: float
    total_latency_ms: float
    audio_samples: int
    text_length: int
    mode: str  # "push_to_talk" or "hands_free"
    model: str
    success: bool

class PerformanceTracker:
    """
    Track and analyze performance metrics
    - Detailed timing breakdowns
    - Statistical analysis
    - Performance guardrails
    """
    
    def __init__(self,
                 target_latency_ms: float = 500,
                 log_file: Optional[str] = None,
                 print_summary_every: int = 10):
        """
        Initialize performance tracker
        
        Args:
            target_latency_ms: Target end-to-end latency
            log_file: Optional file to log metrics
            print_summary_every: Print summary every N transcriptions
        """
        self.target_latency_ms = target_latency_ms
        self.log_file = log_file
        self.print_summary_every = print_summary_every
        
        self.metrics: List[TranscriptionMetrics] = []
        self.session_start = time.time()
        
        # Component timers
        self.timers: Dict[str, float] = {}
    
    def start_timer(self, name: str):
        """Start a named timer"""
        self.timers[name] = time.perf_counter()
    
    def end_timer(self, name: str) -> float:
        """End a named timer and return duration in ms"""
        if name not in self.timers:
            return 0.0
        duration = (time.perf_counter() - self.timers[name]) * 1000
        del self.timers[name]
        return duration
    
    def add_transcription(self,
                         recording_duration_ms: float,
                         vad_processing_ms: float,
                         audio_processing_ms: float,
                         transcription_ms: float,
                         text_insertion_ms: float,
                         audio_samples: int,
                         text: str,
                         mode: str,
                         model: str,
                         success: bool = True):
        """Add a transcription metric"""
        # Calculate total latency (excluding recording time)
        total_latency = (vad_processing_ms + audio_processing_ms + 
                        transcription_ms + text_insertion_ms)
        
        metric = TranscriptionMetrics(
            timestamp=time.time(),
            recording_duration_ms=recording_duration_ms,
            vad_processing_ms=vad_processing_ms,
            audio_processing_ms=audio_processing_ms,
            transcription_ms=transcription_ms,
            text_insertion_ms=text_insertion_ms,
            total_latency_ms=total_latency,
            audio_samples=audio_samples,
            text_length=len(text) if text else 0,
            mode=mode,
            model=model,
            success=success
        )
        
        self.metrics.append(metric)
        
        # Log to file if configured
        if self.log_file:
            self._log_metric(metric)
        
        # Print summary if needed
        if len(self.metrics) % self.print_summary_every == 0:
            self.print_summary()
        
        # Check performance guardrail
        if total_latency > self.target_latency_ms:
            print(f"⚠️  Latency exceeded target: {total_latency:.0f}ms > {self.target_latency_ms:.0f}ms", 
                  file=sys.stderr)
    
    def _log_metric(self, metric: TranscriptionMetrics):
        """Log metric to file"""
        try:
            with open(self.log_file, 'a') as f:
                f.write(json.dumps(asdict(metric)) + '\n')
        except Exception as e:
            print(f"Failed to log metric: {e}", file=sys.stderr)
    
    def get_statistics(self) -> Dict:
        """Calculate performance statistics"""
        if not self.metrics:
            return {}
        
        # Extract latencies
        latencies = [m.total_latency_ms for m in self.metrics if m.success]
        if not latencies:
            return {}
        
        # Calculate percentiles
        p50 = np.percentile(latencies, 50)
        p95 = np.percentile(latencies, 95)
        p99 = np.percentile(latencies, 99)
        
        # Component breakdowns
        vad_times = [m.vad_processing_ms for m in self.metrics if m.vad_processing_ms > 0]
        audio_times = [m.audio_processing_ms for m in self.metrics]
        transcription_times = [m.transcription_ms for m in self.metrics]
        insertion_times = [m.text_insertion_ms for m in self.metrics]
        
        return {
            'total_transcriptions': len(self.metrics),
            'successful_transcriptions': sum(1 for m in self.metrics if m.success),
            'latency_p50_ms': p50,
            'latency_p95_ms': p95,
            'latency_p99_ms': p99,
            'latency_mean_ms': np.mean(latencies),
            'latency_min_ms': np.min(latencies),
            'latency_max_ms': np.max(latencies),
            'meeting_target_pct': sum(1 for l in latencies if l <= self.target_latency_ms) / len(latencies) * 100,
            'component_breakdown': {
                'vad_mean_ms': np.mean(vad_times) if vad_times else 0,
                'audio_mean_ms': np.mean(audio_times),
                'transcription_mean_ms': np.mean(transcription_times),
                'insertion_mean_ms': np.mean(insertion_times),
            },
            'mode_breakdown': {
                'push_to_talk': sum(1 for m in self.metrics if m.mode == 'push_to_talk'),
                'hands_free': sum(1 for m in self.metrics if m.mode == 'hands_free'),
            }
        }
    
    def print_summary(self):
        """Print performance summary"""
        stats = self.get_statistics()
        if not stats:
            return
        
        print("\n" + "="*60, file=sys.stderr)
        print("📊 PERFORMANCE SUMMARY", file=sys.stderr)
        print("="*60, file=sys.stderr)
        
        print(f"Total: {stats['total_transcriptions']} transcriptions "
              f"({stats['successful_transcriptions']} successful)", file=sys.stderr)
        
        print(f"\n⏱️  Latency (excluding recording):", file=sys.stderr)
        print(f"  P50: {stats['latency_p50_ms']:.0f}ms", file=sys.stderr)
        print(f"  P95: {stats['latency_p95_ms']:.0f}ms", file=sys.stderr)
        print(f"  P99: {stats['latency_p99_ms']:.0f}ms", file=sys.stderr)
        print(f"  Mean: {stats['latency_mean_ms']:.0f}ms", file=sys.stderr)
        print(f"  Range: {stats['latency_min_ms']:.0f}ms - {stats['latency_max_ms']:.0f}ms", file=sys.stderr)
        
        if stats['meeting_target_pct'] < 95:
            print(f"  ⚠️  Meeting target ({self.target_latency_ms}ms): {stats['meeting_target_pct']:.1f}%", 
                  file=sys.stderr)
        else:
            print(f"  ✅ Meeting target ({self.target_latency_ms}ms): {stats['meeting_target_pct']:.1f}%", 
                  file=sys.stderr)
        
        print(f"\n🔧 Component breakdown (mean):", file=sys.stderr)
        breakdown = stats['component_breakdown']
        if breakdown['vad_mean_ms'] > 0:
            print(f"  VAD: {breakdown['vad_mean_ms']:.0f}ms", file=sys.stderr)
        print(f"  Audio: {breakdown['audio_mean_ms']:.0f}ms", file=sys.stderr)
        print(f"  Transcription: {breakdown['transcription_mean_ms']:.0f}ms", file=sys.stderr)
        print(f"  Insertion: {breakdown['insertion_mean_ms']:.0f}ms", file=sys.stderr)
        
        mode_breakdown = stats['mode_breakdown']
        if mode_breakdown['hands_free'] > 0:
            print(f"\n📱 Mode usage:", file=sys.stderr)
            print(f"  Push-to-talk: {mode_breakdown['push_to_talk']}", file=sys.stderr)
            print(f"  Hands-free: {mode_breakdown['hands_free']}", file=sys.stderr)
        
        print("="*60 + "\n", file=sys.stderr)
    
    def export_metrics(self, output_file: str):
        """Export all metrics to JSON file"""
        data = {
            'session_start': self.session_start,
            'session_duration_s': time.time() - self.session_start,
            'target_latency_ms': self.target_latency_ms,
            'statistics': self.get_statistics(),
            'metrics': [asdict(m) for m in self.metrics]
        }
        
        with open(output_file, 'w') as f:
            json.dump(data, f, indent=2)
        
        print(f"📊 Metrics exported to: {output_file}", file=sys.stderr)
    
    def print_optimization_tips(self):
        """Print optimization tips based on metrics"""
        stats = self.get_statistics()
        if not stats:
            return
        
        print("\n💡 OPTIMIZATION TIPS", file=sys.stderr)
        print("="*60, file=sys.stderr)
        
        breakdown = stats['component_breakdown']
        
        # Find bottleneck
        bottleneck = max(breakdown.items(), key=lambda x: x[1])
        component, time_ms = bottleneck
        
        if component == 'transcription_mean_ms' and time_ms > 500:
            print("• Transcription is the bottleneck:", file=sys.stderr)
            print("  - Try a smaller model (base.en or tiny.en)", file=sys.stderr)
            print("  - Ensure Metal acceleration is working", file=sys.stderr)
            print("  - Check CPU usage during transcription", file=sys.stderr)
        elif component == 'audio_mean_ms' and time_ms > 50:
            print("• Audio processing is slow:", file=sys.stderr)
            print("  - Ensure 16kHz direct recording", file=sys.stderr)
            print("  - Reduce buffer size if possible", file=sys.stderr)
        elif component == 'insertion_mean_ms' and time_ms > 50:
            print("• Text insertion is slow:", file=sys.stderr)
            print("  - Check if Accessibility API is working", file=sys.stderr)
            print("  - Try reducing paste delay", file=sys.stderr)
        elif component == 'vad_mean_ms' and time_ms > 30:
            print("• VAD processing is slow:", file=sys.stderr)
            print("  - Reduce VAD aggressiveness", file=sys.stderr)
            print("  - Increase frame size to 30ms", file=sys.stderr)
        
        if stats['latency_p95_ms'] > stats['latency_p50_ms'] * 2:
            print("\n• High variance detected (P95 >> P50):", file=sys.stderr)
            print("  - Check for background processes", file=sys.stderr)
            print("  - Consider model warmup", file=sys.stderr)
            print("  - Pin threads to performance cores", file=sys.stderr)
        
        print("="*60, file=sys.stderr)