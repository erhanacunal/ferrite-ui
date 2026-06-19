; image v4: 12 functions, 7 globals, opcodes=506B, render=dirty, widgets=0, exts=0
;   #1 gpiob_input  offset=147 length=7
;   #2 gpiob_output  offset=154 length=7
;   #3 gpiob_read  offset=161 length=6
;   #4 gpiob_write  offset=167 length=8
;   #5 fpga_cmd_only  offset=175 length=8
;   #6 fpga_clock  offset=183 length=6
;   #7 usart_line  offset=189 length=13
;   #8 set_addr  offset=202 length=25
;   #9 write_pixel_word  offset=227 length=21
;   #10 read_probe_word  offset=248 length=126
;   #11 setup  offset=0 length=147
;   #12 loop  offset=374 length=132

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
0017: PUSH 100
0019: setBrightness
001A: PUSH_0
001B: PUSH 52429280 (0x032001E0)
0020: PUSH_0
0021: fillRect
0022: PUSH 1310750 (0x0014001E)
0027: PUSH_0
0028: PUSH -65536 (0xFFFF0000)
002D: drawTextLit "FPGA CMD 0x0E READ PROBE"
0047: strLit "fpga_read_probe start"
005E: CALL @00BD
0061: POP
0062: PUSH 20
0064: PUSH 20
0066: PUSH 63488 (0x0000F800)
006B: CALL @00E3
006E: POP
006F: PUSH 21
0071: PUSH 20
0073: PUSH 2016 (0x07E0)
0076: CALL @00E3
0079: POP
007A: PUSH 22
007C: PUSH 20
007E: PUSH 31
0080: CALL @00E3
0083: POP
0084: PUSH 23
0086: PUSH 20
0088: PUSH 65535 (0x0000FFFF)
008D: CALL @00E3
0090: POP
0091: PUSH_0
0092: RET

; --- gpiob_input() ---
0093: FRAME 0
0095: PUSH_0
0096: syscall id=0x30 argc=1
0099: RET

; --- gpiob_output() ---
009A: FRAME 0
009C: PUSH_1
009D: syscall id=0x30 argc=1
00A0: RET

; --- gpiob_read() ---
00A1: FRAME 0
00A3: syscall id=0x31 argc=0
00A6: RET

; --- gpiob_write() ---
00A7: FRAME 1
00A9: STORE value
00AA: LOAD value
00AB: syscall id=0x32 argc=1
00AE: RET

; --- fpga_cmd_only() ---
00AF: FRAME 1
00B1: STORE cmd
00B2: LOAD cmd
00B3: syscall id=0x33 argc=1
00B6: RET

; --- fpga_clock() ---
00B7: FRAME 0
00B9: syscall id=0x34 argc=0
00BC: RET

; --- usart_line() ---
00BD: FRAME 1
00BF: STORE s
00C0: LOAD s
00C1: sendUsartStr
00C2: strLit "\r
"
00C7: sendUsartStr
00C8: PUSH_0
00C9: RET

; --- set_addr() ---
00CA: FRAME 2
00CC: STORE py
00CD: STORE px
00CE: CALL @009A
00D1: POP
00D2: PUSH 3
00D4: LOAD px
00D5: fpgaCmd
00D6: PUSH_2
00D7: LOAD py
00D8: fpgaCmd
00D9: PUSH 7
00DB: LOAD px
00DC: fpgaCmd
00DD: PUSH 6
00DF: LOAD py
00E0: fpgaCmd
00E1: PUSH_0
00E2: RET

; --- write_pixel_word() ---
00E3: FRAME 3
00E5: STORE color
00E6: STORE py
00E7: STORE px
00E8: LOAD px
00E9: LOAD py
00EA: CALL @00CA
00ED: POP
00EE: PUSH 15
00F0: CALL @00AF
00F3: POP
00F4: LOAD color
00F5: fpgaData
00F6: PUSH_0
00F7: RET

; --- read_probe_word() ---
00F8: FRAME 2
00FA: STORE py
00FB: STORE px
00FC: LOAD px
00FD: LOAD py
00FE: CALL @00CA
0101: POP
0102: PUSH 14
0104: CALL @00AF
0107: POP
0108: CALL @0093
010B: POP
010C: PUSH_1
010D: delay
010E: CALL @00A1
0111: STORE v0
0113: CALL @00B7
0116: POP
0117: CALL @00A1
011A: STORE v1
011C: CALL @00B7
011F: POP
0120: CALL @00A1
0123: STORE v2
0125: CALL @009A
0128: POP
0129: strLit "READ0E x=%d y=%d expect=0x%X r0=0x%X r1=0x%X r2=0x%X"
015F: LOAD px
0160: LOAD py
0161: LOAD expected
0163: LOAD v0
0165: LOAD v1
0167: LOAD v2
0169: sprintf argc=6
016B: STORE msg
016D: LOAD msg
016F: CALL @00BD
0172: POP
0173: strClear
0174: PUSH_0
0175: RET

; --- loop() ---
0176: FRAME 0
0178: PUSH 20
017A: STORE x
017C: PUSH 20
017E: STORE y
0180: PUSH 63488 (0x0000F800)
0185: STORE expected
0187: LOAD x
0189: LOAD y
018B: CALL @00F8
018E: POP
018F: PUSH 21
0191: STORE x
0193: PUSH 20
0195: STORE y
0197: PUSH 2016 (0x07E0)
019A: STORE expected
019C: LOAD x
019E: LOAD y
01A0: CALL @00F8
01A3: POP
01A4: PUSH 22
01A6: STORE x
01A8: PUSH 20
01AA: STORE y
01AC: PUSH 31
01AE: STORE expected
01B0: LOAD x
01B2: LOAD y
01B4: CALL @00F8
01B7: POP
01B8: PUSH 23
01BA: STORE x
01BC: PUSH 20
01BE: STORE y
01C0: PUSH 65535 (0x0000FFFF)
01C5: STORE expected
01C7: LOAD x
01C9: LOAD y
01CB: CALL @00F8
01CE: POP
01CF: strLit "fpga_read_probe pass complete"
01EE: CALL @00BD
01F1: POP
01F2: PUSH 1000 (0x03E8)
01F5: delay
01F6: YIELD
01F7: JMP @0178
