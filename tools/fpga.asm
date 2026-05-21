; image v4: 12 functions, 8 globals, opcodes=987B, render=buffered, widgets=0, exts=0
;   #1 gpiob_input  offset=106 length=7
;   #2 gpiob_output  offset=113 length=7
;   #3 gpiob_read  offset=120 length=6
;   #4 gpiob_write  offset=126 length=8
;   #5 fpga_cmd_only  offset=134 length=8
;   #6 fpga_clock  offset=142 length=6
;   #7 usart_line  offset=148 length=14
;   #8 set_addr  offset=162 length=25
;   #9 write_pixel_word  offset=187 length=118
;   #10 read_probe_word  offset=305 length=339
;   #11 setup  offset=0 length=106
;   #12 loop  offset=644 length=343

; --- setup() ---
0000: FRAME 0
0002: PUSH_0
0003: STORE x
0005: PUSH_0
0006: STORE y
0008: PUSH_0
0009: STORE expected
000B: PUSH_0
000C: STORE v0
000E: PUSH_0
000F: STORE v1
0011: PUSH_0
0012: STORE v2
0014: PUSH_0
0015: STORE msg
0017: PUSH_0
0018: STORE pass
001A: PUSH 100
001C: setBrightness
001D: PUSH_0
001E: PUSH 52429280 (0x032001E0)
0023: PUSH_0
0024: fillRect
0025: PUSH 1310750 (0x0014001E)
002A: PUSH_0
002B: PUSH -65536 (0xFFFF0000)
0030: drawTextLit "FPGA CMD 0x0E READ PROBE"
004A: strLit "fpga_read_probe start"
0061: CALL @0094
0064: POP
0065: PUSH_0
0066: STORE pass
0068: PUSH_0
0069: RET

; --- gpiob_input() ---
006A: FRAME 0
006C: PUSH_0
006D: syscall id=0x30 argc=1
0070: RET

; --- gpiob_output() ---
0071: FRAME 0
0073: PUSH_1
0074: syscall id=0x30 argc=1
0077: RET

; --- gpiob_read() ---
0078: FRAME 0
007A: syscall id=0x31 argc=0
007D: RET

; --- gpiob_write() ---
007E: FRAME 1
0080: STORE value
0081: LOAD value
0082: syscall id=0x32 argc=1
0085: RET

; --- fpga_cmd_only() ---
0086: FRAME 1
0088: STORE cmd
0089: LOAD cmd
008A: syscall id=0x33 argc=1
008D: RET

; --- fpga_clock() ---
008E: FRAME 0
0090: syscall id=0x34 argc=0
0093: RET

; --- usart_line() ---
0094: FRAME 1
0096: STORE s
0097: LOAD s
0098: sendUsartStr
0099: strLit "\r
"
009E: sendUsartStr
009F: strClear
00A0: PUSH_0
00A1: RET

; --- set_addr() ---
00A2: FRAME 2
00A4: STORE py
00A5: STORE px
00A6: CALL @0071
00A9: POP
00AA: PUSH 3
00AC: LOAD px
00AD: fpgaCmd
00AE: PUSH_2
00AF: LOAD py
00B0: fpgaCmd
00B1: PUSH 7
00B3: LOAD px
00B4: fpgaCmd
00B5: PUSH 6
00B7: LOAD py
00B8: fpgaCmd
00B9: PUSH_0
00BA: RET

; --- write_pixel_word() ---
00BB: FRAME 3
00BD: STORE color
00BE: STORE py
00BF: STORE px
00C0: strLit "write_pixel: set_addr"
00D7: CALL @0094
00DA: POP
00DB: LOAD px
00DC: LOAD py
00DD: CALL @00A2
00E0: POP
00E1: strLit "write_pixel: cmd0f"
00F5: CALL @0094
00F8: POP
00F9: PUSH 15
00FB: CALL @0086
00FE: POP
00FF: strLit "write_pixel: data"
0112: CALL @0094
0115: POP
0116: LOAD color
0117: fpgaData
0118: strLit "write_pixel: done"
012B: CALL @0094
012E: POP
012F: PUSH_0
0130: RET

; --- read_probe_word() ---
0131: FRAME 2
0133: STORE py
0134: STORE px
0135: strLit "read_probe: set_addr"
014B: CALL @0094
014E: POP
014F: LOAD px
0150: LOAD py
0151: CALL @00A2
0154: POP
0155: strLit "read_probe: cmd0e"
0168: CALL @0094
016B: POP
016C: PUSH 14
016E: CALL @0086
0171: POP
0172: strLit "read_probe: input"
0185: CALL @0094
0188: POP
0189: CALL @006A
018C: POP
018D: PUSH_1
018E: delay
018F: strLit "read_probe: read0"
01A2: CALL @0094
01A5: POP
01A6: CALL @0078
01A9: STORE v0
01AB: strLit "read_probe: clock1"
01BF: CALL @0094
01C2: POP
01C3: CALL @008E
01C6: POP
01C7: strLit "read_probe: read1"
01DA: CALL @0094
01DD: POP
01DE: CALL @0078
01E1: STORE v1
01E3: strLit "read_probe: clock2"
01F7: CALL @0094
01FA: POP
01FB: CALL @008E
01FE: POP
01FF: strLit "read_probe: read2"
0212: CALL @0094
0215: POP
0216: CALL @0078
0219: STORE v2
021B: strLit "read_probe: output"
022F: CALL @0094
0232: POP
0233: CALL @0071
0236: POP
0237: strLit "READ0E x=%d y=%d expect=0x%X r0=0x%X r1=0x%X r2=0x%X"
026D: LOAD px
026E: LOAD py
026F: LOAD expected
0271: LOAD v0
0273: LOAD v1
0275: LOAD v2
0277: sprintf argc=6
0279: STORE msg
027B: LOAD msg
027D: CALL @0094
0280: POP
0281: strClear
0282: PUSH_0
0283: RET

; --- loop() ---
0284: FRAME 0
0286: strLit "loop: pass start"
0298: CALL @0094
029B: POP
029C: LOAD pass
029E: PUSH_0
029F: EQ
02A0: JZ @02BF
02A3: strLit "first loop reached"
02B7: CALL @0094
02BA: POP
02BB: PUSH 250 (0x00FA)
02BE: delay
02BF: strLit "loop: write red"
02D0: CALL @0094
02D3: POP
02D4: PUSH 20
02D6: PUSH 20
02D8: PUSH 63488 (0x0000F800)
02DD: CALL @00BB
02E0: POP
02E1: PUSH 10
02E3: delay
02E4: strLit "loop: write green"
02F7: CALL @0094
02FA: POP
02FB: PUSH 21
02FD: PUSH 20
02FF: PUSH 2016 (0x07E0)
0302: CALL @00BB
0305: POP
0306: PUSH 10
0308: delay
0309: strLit "loop: write blue"
031B: CALL @0094
031E: POP
031F: PUSH 22
0321: PUSH 20
0323: PUSH 31
0325: CALL @00BB
0328: POP
0329: PUSH 10
032B: delay
032C: strLit "loop: write white"
033F: CALL @0094
0342: POP
0343: PUSH 23
0345: PUSH 20
0347: PUSH 65535 (0x0000FFFF)
034C: CALL @00BB
034F: POP
0350: PUSH 10
0352: delay
0353: PUSH 20
0355: STORE x
0357: PUSH 20
0359: STORE y
035B: PUSH 63488 (0x0000F800)
0360: STORE expected
0362: LOAD x
0364: LOAD y
0366: CALL @0131
0369: POP
036A: PUSH 21
036C: STORE x
036E: PUSH 20
0370: STORE y
0372: PUSH 2016 (0x07E0)
0375: STORE expected
0377: LOAD x
0379: LOAD y
037B: CALL @0131
037E: POP
037F: PUSH 22
0381: STORE x
0383: PUSH 20
0385: STORE y
0387: PUSH 31
0389: STORE expected
038B: LOAD x
038D: LOAD y
038F: CALL @0131
0392: POP
0393: PUSH 23
0395: STORE x
0397: PUSH 20
0399: STORE y
039B: PUSH 65535 (0x0000FFFF)
03A0: STORE expected
03A2: LOAD x
03A4: LOAD y
03A6: CALL @0131
03A9: POP
03AA: strLit "fpga_read_probe pass complete"
03C9: CALL @0094
03CC: POP
03CD: LOAD pass
03CF: PUSH_1
03D0: ADD
03D1: STORE pass
03D3: PUSH 1000 (0x03E8)
03D6: delay
03D7: YIELD
03D8: JMP @0286
