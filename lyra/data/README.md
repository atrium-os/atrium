# lyra/data

## hrtf-bed-fabian-48k.bin

The head-related impulse responses lyrad uses for headphone virtual surround
(docs/spec/lyra-spatial.md S1/S3): eight 7.1 bed directions, two ears, 256 taps
at 48 kHz. Format and processing: `scripts/hrtf-bed.py`.

**Source and credit.** Derived from the FABIAN head-related transfer function
database, Fabian Brinkmann, Alexander Lindau, Stefan Weinzierl, Steven van de Par,
Markus Müller-Trapet, Rob Opdam and Michael Vorländer, Technische Universität
Berlin, DOI [10.14279/depositonce-5718.5](https://doi.org/10.14279/depositonce-5718.5),
described in "A High Resolution and Full-Spherical Head-Related Transfer Function
Database for Different Head-Above-Torso Orientations", J. Audio Eng. Soc. 65(10),
2017. Licensed under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).

**Changes made** (from `FABIAN_HRIR_measured_HATO_0.sofa`, SHA-256
`83ebbcd9a09d17679b95d201c9775438c0bb1199d565c3fc7a25448a905cdc3c`, in
`HRIRs_neutral_head_orientation_v4.zip`): the eight bed directions were picked,
diffuse-field equalised with FABIAN's inverted smoothed common transfer function,
resampled from 44.1 to 48 kHz, cut to 256 taps with a fade, and scaled; the LFE row
is a delayed impulse added by us. This derived file is also offered under CC BY 4.0.
