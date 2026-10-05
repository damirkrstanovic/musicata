Synthetic one-second 440 Hz tone for deterministic ADTS AAC radio decoding tests.
Generated with:

```sh
ffmpeg -f lavfi -i sine=frequency=440:sample_rate=44100:duration=1 -c:a aac -b:a 32k -f adts radio.aac
```

This fixture is original generated test material, distributed under the repository license.
